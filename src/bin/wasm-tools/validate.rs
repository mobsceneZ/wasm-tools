use addr2line::LookupResult;
use anyhow::{Context, Result, bail};
use bitflags::Flags;
use rayon::prelude::*;
use std::fmt::Write;
use std::mem;
use std::time::Instant;
use wasm_tools::addr2line::Addr2lineModules;
use wasmparser::{
    BinaryReaderError, FuncValidatorAllocations, Parser, ValidPayload, Validator, WasmFeatures,
};

/// Validate a WebAssembly binary
///
/// This subcommand will validate a WebAssembly binary to determine if it's
/// valid or not. This implements the validation algorithm of the WebAssembly
/// specification. The process will exit with 0 and no output if the binary is
/// valid, or nonzero and an error message on stderr if the binary is not valid.
///
#[derive(clap::Parser)]
#[clap(after_help = "\
Examples:

    # Validate `foo.wasm` with the default Wasm feature proposals.
    $ wasm-tools validate foo.wasm

    # Validate `foo.wasm` with more verbose output
    $ wasm-tools validate -vv foo.wasm

    # Validate `fancy.wasm` with all Wasm feature proposals enabled.
    $ wasm-tools validate --features all fancy.wasm

    # Validate `mvp.wasm` with the original wasm feature set enabled.
    $ wasm-tools validate --features=wasm1 mvp.wasm
    $ wasm-tools validate --features=mvp mvp.wasm
")]
pub struct Opts {
    #[clap(flatten)]
    features: CliFeatures,

    #[clap(flatten)]
    io: wasm_tools::InputOutput,
}

// Helper structure extracted used to parse the feature flags for `validate`.
#[derive(clap::Parser)]
pub struct CliFeatures {
    /// Comma-separated list of WebAssembly features to enable during
    /// validation.
    ///
    /// If a "-" character is present in front of a feature it will disable that
    /// feature. For example "-simd" will disable the simd proposal.
    ///
    /// The placeholder "all" can be used to enable all wasm features and the
    /// term "-all" can be used to disable all features.
    ///
    /// The default set of features enabled are all WebAssembly proposals that
    /// are at phase 4 or after. This means that the default set of features
    /// accepted are relatively bleeding edge. Versions of the WebAssembly
    /// specification can also be selected. The "wasm1" or "mvp" feature can
    /// select the original WebAssembly specification and "wasm2" can be used to
    /// select the 2.0 version.
    ///
    /// Available feature options can be found in the wasmparser crate:
    /// <https://github.com/bytecodealliance/wasm-tools/blob/main/crates/wasmparser/src/features.rs>
    #[clap(long, short = 'f', value_parser = parse_features)]
    features: Vec<Vec<FeatureAction>>,
}

#[derive(Clone)]
enum FeatureAction {
    Reset(WasmFeatures),
    Enable(WasmFeatures),
    Disable(WasmFeatures),
}

impl Opts {
    pub fn general_opts(&self) -> &wasm_tools::GeneralOpts {
        self.io.general_opts()
    }

    pub fn run(&self) -> Result<()> {
        let start = Instant::now();
        let wasm = self.io.get_input_wasm()?; // no need to parse as the validator will do this
        log::info!("read module in {:?}", start.elapsed());

        // If validation fails then try to attach extra information to the
        // error based on DWARF information in the input wasm binary. If
        // DWARF information isn't present or if the DWARF failed to get parsed
        // then ignore the error and carry on.
        let error = match self.validate(&wasm) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let offset = match error.downcast_ref::<BinaryReaderError>() {
            Some(err) => err.offset(),
            None => return Err(error.into()),
        };
        match self.annotate_error_with_file_and_line(&wasm, offset) {
            Ok(Some(msg)) => Err(error.context(msg)),
            Ok(None) => Err(error.into()),
            Err(e) => {
                log::warn!("failed to parse DWARF information: {e:?}");
                Err(error.into())
            }
        }
    }

    fn validate(&self, wasm: &[u8]) -> Result<()> {
        // Note that here we're copying the contents of
        // `Validator::validate_all`, but the end is followed up with a parallel
        // iteration over the functions to validate instead of a synchronous
        // validation.
        //
        // The general idea here is that we're going to use `Parser::parse_all`
        // to divvy up the input bytes into chunks. We'll maintain which
        // `Validator` we're using as we navigate nested modules (the module
        // linking proposal) and any functions found are deferred to get
        // validated later.
        let mut validator = Validator::new_with_features(self.features.features());
        let mut functions_to_validate = Vec::new();

        let start = Instant::now();
        for payload in Parser::new(0).parse_all(&wasm) {
            match validator.payload(&payload?)? {
                ValidPayload::Ok | ValidPayload::Parser(_) | ValidPayload::End(_) => {}
                ValidPayload::Func(validator, body) => {
                    functions_to_validate.push((validator, body))
                }
            }
        }
        log::info!("module structure validated in {:?}", start.elapsed());

        // After we've validate the entire wasm module we'll use `rayon` to
        // iterate over all functions in parallel and perform parallel
        // validation of the input wasm module.
        //
        // Note that validation results for each function are collected into a
        // vector to ensure that in the case of multiple errors the first is
        // always reported. Otherwise `rayon` does not guarantee the order that
        // failures show up in.
        let start = Instant::now();
        functions_to_validate
            .into_par_iter()
            .map_init(
                FuncValidatorAllocations::default,
                |allocs, (to_validate, body)| -> Result<_> {
                    let mut validator = to_validate.into_validator(mem::take(allocs));
                    validator.validate(&body).with_context(|| {
                        format!("func {} failed to validate", validator.index())
                    })?;
                    *allocs = validator.into_allocations();
                    Ok(())
                },
            )
            .collect::<Vec<_>>()
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        log::info!("functions validated in {:?}", start.elapsed());
        Ok(())
    }

    fn annotate_error_with_file_and_line(
        &self,
        wasm: &[u8],
        offset: usize,
    ) -> Result<Option<String>> {
        let mut modules = Addr2lineModules::parse(wasm)?;
        let code_section_relative = false;
        let (context, text_rel) = match modules.context(offset as u64, code_section_relative)? {
            Some(pair) => pair,
            None => return Ok(None),
        };

        let mut frames = match context.find_frames(text_rel) {
            LookupResult::Output(result) => result?,
            LookupResult::Load { .. } => return Ok(None),
        };
        let frame = match frames.next()? {
            Some(frame) => frame,
            None => return Ok(None),
        };

        let mut out = String::new();
        if let Some(loc) = &frame.location {
            if let Some(file) = loc.file {
                write!(out, "{file}")?;
            }
            if let Some(line) = loc.line {
                write!(out, ":{line}")?;
            }
            if let Some(column) = loc.column {
                write!(out, ":{column}")?;
            }
            write!(out, " ")?;
        }
        if let Some(func) = &frame.function {
            write!(out, "function `{}` failed to validate", func.demangle()?)?;
        }

        if out.is_empty() {
            Ok(None)
        } else {
            Ok(Some(out))
        }
    }
}

impl CliFeatures {
    pub fn features(&self) -> WasmFeatures {
        let mut ret = WasmFeatures::default();

        for action in self.features.iter().flat_map(|v| v) {
            match action {
                FeatureAction::Enable(features) => {
                    ret |= *features;
                }
                FeatureAction::Disable(features) => {
                    ret &= !*features;
                }
                FeatureAction::Reset(features) => {
                    ret = *features;
                }
            }
        }

        ret
    }
}

fn parse_features(arg: &str) -> Result<Vec<FeatureAction>> {
    let mut ret = Vec::new();

    const GROUPS: &[(&str, WasmFeatures)] = &[
        ("mvp", WasmFeatures::MVP),
        ("wasm1", WasmFeatures::WASM1),
        ("wasm2", WasmFeatures::WASM2),
        ("wasm3", WasmFeatures::WASM3),
        ("lime1", WasmFeatures::LIME1),
    ];

    enum Action {
        ChangeAll,
        Group(WasmFeatures),
        Modify(WasmFeatures),
    }

    fn actions() -> impl Iterator<Item = (&'static str, Action)> {
        WasmFeatures::FLAGS
            .iter()
            .map(|f| (f.name(), Action::Modify(*f.value())))
            .chain(
                GROUPS
                    .iter()
                    .map(|(name, features)| (*name, Action::Group(*features))),
            )
            .chain([("all", Action::ChangeAll)])
    }

    fn flag_name(name: &str) -> String {
        name.to_lowercase().replace('_', "-")
    }

    'outer: for part in arg.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let (enable, part) = if let Some(part) = part.strip_prefix("-") {
            (false, part)
        } else {
            (true, part)
        };
        for (name, action) in actions() {
            if part != flag_name(name) {
                continue;
            }
            match action {
                Action::ChangeAll => {
                    ret.push(if enable {
                        FeatureAction::Enable(WasmFeatures::all())
                    } else {
                        FeatureAction::Disable(WasmFeatures::all())
                    });
                }
                Action::Modify(feature) => {
                    ret.push(if enable {
                        FeatureAction::Enable(feature)
                    } else {
                        FeatureAction::Disable(feature)
                    });
                }
                Action::Group(features) => {
                    if !enable {
                        bail!("cannot disable `{part}`, it can only be enabled");
                    }
                    ret.push(FeatureAction::Reset(features));
                }
            }
            continue 'outer;
        }

        let mut error = format!("unknown feature `{part}`\n");
        error.push_str("Valid features: ");
        let mut first = true;
        for (name, _) in actions() {
            if first {
                first = false;
            } else {
                error.push_str(", ");
            }
            error.push_str(&flag_name(name));
        }
        bail!("{error}")
    }

    Ok(ret)
}
