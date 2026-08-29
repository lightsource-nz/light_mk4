//! Modules, and the runtime that loads and polls them.
//!
//! Registration is EXPLICIT. An application constructs its modules as values and adds each one
//! to the runtime; the runtime orders them by their declared dependencies and loads them in that
//! order. There is no linker-section walk, no `used` attribute holding the whole thing together,
//! and no per-port linker script -- the three things mk3's registration needed and the one that
//! silently broke at -O1 until the attribute was found. A dependency that was never added is an
//! error at start-up with the name in it, not a module that quietly loads without it.
//!
//! Modules own their state. The runtime holds `&mut` to each for as long as it runs, which is
//! how a module's poll can mutate it without a lock, and how two modules cannot alias each
//! other's state by construction.

use heapless::Vec;

/// What a module's poll reports back to the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
        /// Nothing to do right now.
        Idle,
        /// Did work; poll again soon.
        Busy,
        /// Ask the runtime to stop and unload everything.
        Shutdown,
        /// Something went wrong that the module cannot recover from.
        Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
        /// The runtime's fixed capacity is full. Raise `N`, or register fewer modules.
        Capacity,
        /// A module names a dependency nobody registered.
        MissingDependency { module: &'static str, dep: &'static str },
        /// A dependency cycle reaches this module.
        Cycle(&'static str),
        /// A module's `load` failed.
        Load(&'static str),
        /// `run` was called before `start`, or `start` twice.
        NotStarted,
        /// A module returned `Poll::Error`.
        Module(&'static str),
}

pub trait Module {
        /// Unique within one application; also what `deps` refers to.
        fn name(&self) -> &'static str;

        /// Names of modules that must load before this one.
        fn deps(&self) -> &'static [&'static str] {
                &[]
        }

        /// Called once, after every dependency has loaded.
        fn load(&mut self) -> Result<(), ()> {
                Ok(())
        }

        /// Called once at shutdown, in reverse load order.
        fn unload(&mut self) {}

        /// Called repeatedly while the application runs, in load order.
        fn poll(&mut self) -> Poll {
                Poll::Idle
        }
}

/// The application's module set, ordered and driven.
///
/// `N` is the capacity. Unlike mk3's `LF_STATIC_MODULES_MAX`, exceeding it is a value the caller
/// sees rather than a silently dropped module.
pub struct Runtime<'a, const N: usize> {
        modules: Vec<&'a mut dyn Module, N>,
        /// Indices into `modules`, in load order. Empty until `start`.
        order: Vec<usize, N>,
        /// How many of `order` have been loaded (so a failed start unloads only those).
        loaded: usize,
}

impl<'a, const N: usize> Default for Runtime<'a, N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<'a, const N: usize> Runtime<'a, N> {
        pub const fn new() -> Self {
                Self { modules: Vec::new(), order: Vec::new(), loaded: 0 }
        }

        /// Register a module. Order of registration only matters as a tie-break among modules
        /// that do not depend on each other.
        pub fn add(&mut self, module: &'a mut dyn Module) -> Result<(), Error> {
                self.modules.push(module).map_err(|_| Error::Capacity)
        }

        /// Resolve dependencies and load every module in dependency order.
        pub fn start(&mut self) -> Result<(), Error> {
                if !self.order.is_empty() {
                        return Err(Error::NotStarted);
                }
                self.resolve_order()?;
                for i in 0..self.order.len() {
                        let idx = self.order[i];
                        let m = &mut self.modules[idx];
                        if m.load().is_err() {
                                let name = m.name();
                                self.unload_loaded();
                                return Err(Error::Load(name));
                        }
                        self.loaded += 1;
                }
                Ok(())
        }

        /// Poll every module once, in load order. `Shutdown` and `Error` from any module end
        /// the pass; the rest are summarised as `Busy` if anyone was.
        pub fn poll_once(&mut self) -> Result<Poll, Error> {
                if self.order.is_empty() {
                        return Err(Error::NotStarted);
                }
                let mut result = Poll::Idle;
                for i in 0..self.order.len() {
                        let m = &mut self.modules[self.order[i]];
                        match m.poll() {
                                Poll::Idle => {}
                                Poll::Busy => result = Poll::Busy,
                                Poll::Shutdown => return Ok(Poll::Shutdown),
                                Poll::Error => return Err(Error::Module(m.name())),
                        }
                }
                Ok(result)
        }

        /// Poll until a module asks for shutdown or fails, then unload everything in reverse
        /// load order. `idle` is called between passes in which nothing was busy -- the place
        /// a port sleeps or waits for an interrupt.
        pub fn run(&mut self, mut idle: impl FnMut()) -> Result<(), Error> {
                let result = loop {
                        match self.poll_once() {
                                Ok(Poll::Shutdown) => break Ok(()),
                                Ok(Poll::Idle) => idle(),
                                Ok(_) => {}
                                Err(e) => break Err(e),
                        }
                };
                self.unload_loaded();
                result
        }

        /// Names in load order; empty before `start`.
        pub fn load_order(&self) -> impl Iterator<Item = &'static str> + '_ {
                self.order.iter().map(|&i| self.modules[i].name())
        }

        fn unload_loaded(&mut self) {
                while self.loaded > 0 {
                        self.loaded -= 1;
                        let idx = self.order[self.loaded];
                        self.modules[idx].unload();
                }
                self.order.clear();
        }

        /// Depth-first over declared dependencies, in registration order, so the result is the
        /// same every start. A `Visiting` mark on the current path is how a cycle is told
        /// apart from a diamond.
        fn resolve_order(&mut self) -> Result<(), Error> {
                #[derive(Clone, Copy, PartialEq)]
                enum Mark {
                        Unvisited,
                        Visiting,
                        Done,
                }
                let mut marks = [Mark::Unvisited; N];
                //   an explicit stack rather than recursion: N is small but the stack is a
                // fixed budget on a microcontroller, and an iterative walk is bounded by N
                let mut stack: Vec<(usize, usize), N> = Vec::new();

                for root in 0..self.modules.len() {
                        if marks[root] != Mark::Unvisited {
                                continue;
                        }
                        marks[root] = Mark::Visiting;
                        let _ = stack.push((root, 0));
                        while let Some(&(idx, next_dep)) = stack.last() {
                                let deps = self.modules[idx].deps();
                                if next_dep < deps.len() {
                                        stack.last_mut().unwrap().1 += 1;
                                        let dep_name = deps[next_dep];
                                        let dep_idx = self
                                                .modules
                                                .iter()
                                                .position(|m| m.name() == dep_name)
                                                .ok_or(Error::MissingDependency {
                                                        module: self.modules[idx].name(),
                                                        dep: dep_name,
                                                })?;
                                        match marks[dep_idx] {
                                                Mark::Done => {}
                                                Mark::Visiting => return Err(Error::Cycle(dep_name)),
                                                Mark::Unvisited => {
                                                        marks[dep_idx] = Mark::Visiting;
                                                        let _ = stack.push((dep_idx, 0));
                                                }
                                        }
                                } else {
                                        stack.pop();
                                        marks[idx] = Mark::Done;
                                        let _ = self.order.push(idx);
                                }
                        }
                }
                Ok(())
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::rc::Rc;
        use std::vec::Vec as StdVec;

        extern crate std;

        /// Records load/unload/poll events into a shared journal so order can be asserted.
        struct Probe {
                name: &'static str,
                deps: &'static [&'static str],
                journal: Rc<RefCell<StdVec<std::string::String>>>,
                fail_load: bool,
                polls_until_shutdown: Option<u32>,
        }

        impl Probe {
                fn new(name: &'static str, deps: &'static [&'static str], journal: &Rc<RefCell<StdVec<std::string::String>>>) -> Self {
                        Self { name, deps, journal: journal.clone(), fail_load: false, polls_until_shutdown: None }
                }
                fn note(&self, what: &str) {
                        self.journal.borrow_mut().push(std::format!("{}:{}", what, self.name));
                }
        }

        impl Module for Probe {
                fn name(&self) -> &'static str {
                        self.name
                }
                fn deps(&self) -> &'static [&'static str] {
                        self.deps
                }
                fn load(&mut self) -> Result<(), ()> {
                        self.note("load");
                        if self.fail_load { Err(()) } else { Ok(()) }
                }
                fn unload(&mut self) {
                        self.note("unload");
                }
                fn poll(&mut self) -> Poll {
                        match self.polls_until_shutdown {
                                Some(0) => Poll::Shutdown,
                                Some(n) => {
                                        self.polls_until_shutdown = Some(n - 1);
                                        Poll::Busy
                                }
                                None => Poll::Idle,
                        }
                }
        }

        fn journal() -> Rc<RefCell<StdVec<std::string::String>>> {
                Rc::new(RefCell::new(StdVec::new()))
        }

        #[test]
        fn loads_dependencies_first_and_unloads_in_reverse() {
                let j = journal();
                //   registered app-first, the way an application would list them, with a
                // diamond: app -> {ui, cli} -> core
                let mut app = Probe::new("app", &["ui", "cli"], &j);
                let mut ui = Probe::new("ui", &["core"], &j);
                let mut cli = Probe::new("cli", &["core"], &j);
                let mut core = Probe::new("core", &[], &j);
                app.polls_until_shutdown = Some(2);

                let mut rt: Runtime<8> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut ui).unwrap();
                rt.add(&mut cli).unwrap();
                rt.add(&mut core).unwrap();
                rt.start().unwrap();
                let order: StdVec<_> = rt.load_order().collect();
                assert_eq!(order, ["core", "ui", "cli", "app"]);
                rt.run(|| {}).unwrap();

                let j = j.borrow();
                let unloads: StdVec<_> = j.iter().filter(|e| e.starts_with("unload")).collect();
                assert_eq!(unloads, ["unload:app", "unload:cli", "unload:ui", "unload:core"]);
        }

        #[test]
        fn a_missing_dependency_is_named() {
                let j = journal();
                let mut app = Probe::new("app", &["display"], &j);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                assert_eq!(
                        rt.start(),
                        Err(Error::MissingDependency { module: "app", dep: "display" })
                );
                assert!(j.borrow().is_empty(), "nothing loads when resolution fails");
        }

        #[test]
        fn a_cycle_is_an_error_not_a_hang() {
                let j = journal();
                let mut a = Probe::new("a", &["b"], &j);
                let mut b = Probe::new("b", &["c"], &j);
                let mut c = Probe::new("c", &["a"], &j);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut a).unwrap();
                rt.add(&mut b).unwrap();
                rt.add(&mut c).unwrap();
                assert!(matches!(rt.start(), Err(Error::Cycle(_))));
        }

        #[test]
        fn a_failed_load_unloads_what_already_loaded() {
                let j = journal();
                let mut app = Probe::new("app", &["core"], &j);
                let mut core = Probe::new("core", &[], &j);
                app.fail_load = true;
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut core).unwrap();
                assert_eq!(rt.start(), Err(Error::Load("app")));
                assert_eq!(*j.borrow(), ["load:core", "load:app", "unload:core"]);
        }

        #[test]
        fn capacity_is_an_error_not_a_silent_drop() {
                let j = journal();
                let mut a = Probe::new("a", &[], &j);
                let mut b = Probe::new("b", &[], &j);
                let mut rt: Runtime<1> = Runtime::new();
                rt.add(&mut a).unwrap();
                assert_eq!(rt.add(&mut b), Err(Error::Capacity));
        }

        #[test]
        fn idle_runs_only_when_nothing_was_busy() {
                let j = journal();
                let mut app = Probe::new("app", &[], &j);
                let mut quiet = Probe::new("quiet", &[], &j);
                app.polls_until_shutdown = Some(3);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut quiet).unwrap();
                rt.start().unwrap();
                let mut idles = 0;
                rt.run(|| idles += 1).unwrap();
                assert_eq!(idles, 0, "app was busy on every pass before shutting down");
        }
}
