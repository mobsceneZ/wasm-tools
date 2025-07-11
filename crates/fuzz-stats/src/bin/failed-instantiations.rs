use arbitrary::{Arbitrary, Error, Unstructured};
use rand::RngCore;
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::SeqCst};
use wasmtime::*;

struct State {
    engine: Engine,
    print: bool,
    remaining: AtomicIsize,
    total: AtomicUsize,
    instantiate_trap: AtomicUsize,
    instantiate_oom: AtomicUsize,
}

fn main() {
    Arc::new(State::new()).run();
}

// Theoretically this test can fail because it's based on random data. In
// practice it's expected that the "fails to instantiate" rate is <2%, so if we
// cross the 10% threshold that's quite bad.
#[test]
fn under_10_percent() {
    let mut state = State::new();
    state.print = false;
    state.remaining.store(1000, SeqCst);
    let state = Arc::new(state);
    state.run();

    let total = state.total.load(SeqCst);
    let bad = state.instantiate_trap.load(SeqCst) + state.instantiate_oom.load(SeqCst);
    assert!(
        bad < total / 10,
        "{bad} modules failed to instantiate out of {total}, this failure rate is too high"
    );
}

impl State {
    fn new() -> State {
        State {
            engine: Engine::default(),
            print: true,
            total: AtomicUsize::new(0),
            remaining: AtomicIsize::new(isize::max_value()),
            instantiate_trap: AtomicUsize::new(0),
            instantiate_oom: AtomicUsize::new(0),
        }
    }

    fn run(self: &Arc<Self>) {
        let threads = (0..num_cpus::get())
            .map(|_| {
                let state = self.clone();
                std::thread::spawn(move || state.run_worker())
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
    }

    fn run_worker(&self) {
        let mut rng = rand::rng();
        let mut data = Vec::new();

        while self.remaining.fetch_sub(1, SeqCst) >= 0 {
            data.truncate(0);
            data.resize(1024, 0);
            rng.fill_bytes(&mut data);
            loop {
                match self.run_once(&data) {
                    Ok(()) => break,
                    Err(Error::NotEnoughData) => {
                        let cur = data.len();
                        let extra = 1024;
                        data.resize(cur + extra, 0);
                        rng.fill_bytes(&mut data[cur..]);
                    }
                    Err(e) => panic!("failed to generated module: {e}"),
                }
            }
        }
    }

    /// Generates a random modules using `data`, and then attempts to
    /// instantiate it.
    ///
    /// Records when instantiation fails and why it fails.
    fn run_once(&self, data: &[u8]) -> Result<(), Error> {
        let mut u = Unstructured::new(data);
        // Here swarm testing is used to get hopefully a bit more coverage of
        // interesting states, and we also forcibly disable all `start`
        // functions for now. Not much work has gone into minimizing the traps
        // generated from wasm functions themselves, and this shouldn't be
        // enabled until that's been worked on.
        let mut config = wasm_smith::Config::arbitrary(&mut u)?;
        config.allow_start_export = false;

        config.exceptions_enabled = false; // Not implemented by Wasmtime
        config.threads_enabled = false; // not enabled by default in Wasmtime

        // NB: just added "table64" support to this and wasmtime doesn't
        // implement that yet
        config.memory64_enabled = false;

        // Wasmtime doesn't support these proposals yet.
        config.gc_enabled = false;

        let mut wasm = wasm_smith::Module::new(config, &mut u)?;
        wasm.ensure_termination(10_000).unwrap();
        let wasm = wasm.to_bytes();

        // We install a resource limiter in the store which limits the store to
        // 1gb of memory. That's half the default allocation of memory for
        // libfuzzer-based fuzzers by default, and ideally we're not in a
        // situation where most of the modules are above this threshold.
        let module = match Module::new(&self.engine, &wasm) {
            Ok(m) => m,
            // NB: after bytecodealliance/wasm-tools#1426 wasm-smith is
            // generating modules that Wasmtime can't handle until
            // bytecodealliance/wasmtime#7996 is on crates.io, until that time
            // ignore these errors.
            Err(e) if format!("{e:?}").contains("unsupported init expr") => return Ok(()),
            Err(e) => panic!("unexpected module compile error {e:?}"),
        };
        let mut store = Store::new(
            &self.engine,
            fuzz_stats::limits::StoreLimits {
                remaining_memory: 1 << 30,
                oom: false,
            },
        );
        store.limiter(|s| s as &mut dyn ResourceLimiter);

        // Synthesize dummy imports based on what the module asked for, and then
        // instantiate!
        let instance = fuzz_stats::dummy::dummy_imports(&mut store, &module)
            .and_then(|imports| Instance::new(&mut store, &module, &imports));

        match instance {
            // If instantiation succeeded, we're not too interested in anything
            // else right now. In the future we should probably run exported
            // functions and record whether a trap happened or not.
            Ok(_i) => {}

            Err(e) => {
                // Traps are ok if they happen during instantiation. This is an
                // expected occurrence we want to account for.
                if e.downcast_ref::<Trap>().is_some() {
                    std::fs::write("trap.wasm", &wasm).unwrap();
                    self.instantiate_trap.fetch_add(1, SeqCst);

                // Ooms, like traps, are normal during instantiations. This
                // can happen, for example, if a defined memory is very large.
                } else if store.data().oom {
                    std::fs::write("oom.wasm", &wasm).unwrap();
                    self.instantiate_oom.fetch_add(1, SeqCst);

                // In theory nothing else fails to instantiate. If it does, then
                // panic.
                } else {
                    std::fs::write("panic.wasm", &wasm).unwrap();
                    panic!("unknown: {e}");
                }
            }
        }

        let prev_total = self.total.fetch_add(1, SeqCst);
        if prev_total % 10_000 == 0 && self.print {
            self.print(prev_total + 1);
        }

        Ok(())
    }

    /// Prints summary statistics of how many modules have been instantiated so
    /// far and how many of them have oom'd or trap'd.
    fn print(&self, total: usize) {
        print!("total: {total:8}");
        let stat = |name: &str, stat: &AtomicUsize| {
            let stat = stat.load(SeqCst);
            if stat > 0 {
                print!(" {} {:5.02}% ", name, (stat as f64) / (total as f64) * 100.);
            }
        };
        stat("i-oom", &self.instantiate_oom);
        stat("i-trap", &self.instantiate_trap);
        println!();
    }
}
