//! The root of each single-threaded worker.

use std::rc::Rc;
use std::cell::{RefCell, RefMut};
use std::any::Any;
use std::time::Instant;

use communication::{Allocate, Data, Push, Pull};
use communication::allocator::thread::{ThreadPusher, ThreadPuller};
use scheduling::{Schedule, Scheduler};
use scheduling::activate::Activations;
use progress::timestamp::{Refines};
use progress::{Timestamp, SubgraphBuilder};
use progress::operate::Operate;
use dataflow::scopes::Child;


/// Methods provided by the root Worker.
///
/// These methods are often proxied by child scopes, and this trait provides access.
pub trait AsWorker : Scheduler {

    /// Index of the worker among its peers.
    fn index(&self) -> usize;
    /// Number of peer workers.
    fn peers(&self) -> usize;
    /// Allocates a new channel from a supplied identifier and address.
    ///
    /// The identifier is used to identify the underlying channel and route
    /// its data. It should be distinct from other identifiers passed used
    /// for allocation, but can otherwise be arbitrary.
    ///
    /// The address should specify a path to an operator that should be
    /// scheduled in response to the receipt of records on the channel.
    /// Most commonly, this would be the address of the *target* of the
    /// channel.
    fn allocate<T: Data>(&mut self, identifier: usize, address: &[usize]) -> (Vec<Box<Push<Message<T>>>>, Box<Pull<Message<T>>>);
    /// Constructs a pipeline channel from the worker to itself.
    ///
    /// By default this method uses the native channel allocation mechanism, but the expectation is
    /// that this behavior will be overriden to be more efficient.
    fn pipeline<T: 'static>(&mut self, identifier: usize, address: &[usize]) -> (ThreadPusher<Message<T>>, ThreadPuller<Message<T>>);

    /// Allocates a new worker-unique identifier.
    fn new_identifier(&mut self) -> usize;
    /// Provides access to named logging streams.
    fn log_register(&self) -> ::std::cell::RefMut<::logging_core::Registry<::logging::WorkerIdentifier>>;
    /// Provides access to the timely logging stream.
    fn logging(&self) -> Option<::logging::TimelyLogger> { self.log_register().get("timely") }
}

/// A `Worker` is the entry point to a timely dataflow computation. It wraps a `Allocate`,
/// and has a list of dataflows that it manages.
pub struct Worker<A: Allocate> {
    timer: Instant,
    paths: Rc<RefCell<Vec<Vec<usize>>>>,
    allocator: Rc<RefCell<A>>,
    identifiers: Rc<RefCell<usize>>,
    dataflows: Rc<RefCell<Vec<Wrapper>>>,
    dataflow_counter: Rc<RefCell<usize>>,
    logging: Rc<RefCell<::logging_core::Registry<::logging::WorkerIdentifier>>>,

    activations: Rc<RefCell<Activations>>,
}

impl<A: Allocate> AsWorker for Worker<A> {
    fn index(&self) -> usize { self.allocator.borrow().index() }
    fn peers(&self) -> usize { self.allocator.borrow().peers() }
    fn allocate<D: Data>(&mut self, identifier: usize, address: &[usize]) -> (Vec<Box<Push<Message<D>>>>, Box<Pull<Message<D>>>) {
        if address.len() == 0 { panic!("Unacceptable address: Length zero"); }
        // println!("Exchange allocation; path: {:?}, identifier: {:?}", address, identifier);
        let mut paths = self.paths.borrow_mut();
        while paths.len() <= identifier {
            paths.push(Vec::new());
        }
        paths[identifier] = address.to_vec();
        self.allocator.borrow_mut().allocate(identifier)
    }
    fn pipeline<T: 'static>(&mut self, identifier: usize, address: &[usize]) -> (ThreadPusher<Message<T>>, ThreadPuller<Message<T>>) {
        if address.len() == 0 { panic!("Unacceptable address: Length zero"); }
        // println!("Pipeline allocation; path: {:?}, identifier: {:?}", address, identifier);
        let mut paths = self.paths.borrow_mut();
        while paths.len() <= identifier {
            paths.push(Vec::new());
        }
        paths[identifier] = address.to_vec();
        self.allocator.borrow_mut().pipeline(identifier)
    }

    fn new_identifier(&mut self) -> usize { self.new_identifier() }
    fn log_register(&self) -> RefMut<::logging_core::Registry<::logging::WorkerIdentifier>> {
        self.log_register()
    }
}

impl<A: Allocate> Scheduler for Worker<A> {
    fn activations(&self) -> Rc<RefCell<Activations>> {
        self.activations.clone()
    }
}

impl<A: Allocate> Worker<A> {
    /// Allocates a new `Worker` bound to a channel allocator.
    pub fn new(c: A) -> Worker<A> {
        let now = Instant::now();
        let index = c.index();
        Worker {
            timer: now.clone(),
            paths: Rc::new(RefCell::new(Vec::new())),
            allocator: Rc::new(RefCell::new(c)),
            identifiers: Rc::new(RefCell::new(0)),
            dataflows: Rc::new(RefCell::new(Vec::new())),
            dataflow_counter: Rc::new(RefCell::new(0)),
            logging: Rc::new(RefCell::new(::logging_core::Registry::new(now, index))),
            activations: Rc::new(RefCell::new(Activations::new())),
        }
    }

    /// Performs one step of the computation.
    ///
    /// A step gives each dataflow operator a chance to run, and is the
    /// main way to ensure that a computation proceeds.
    pub fn step(&mut self) -> bool {

        {   // Process channel events. Activate responders.
            let mut allocator = self.allocator.borrow_mut();
            allocator.receive();
            let events = allocator.events().clone();
            let mut borrow = events.borrow_mut();
            for (channel, _event) in borrow.drain(..) {
                // TODO: Pay more attent to `_event`.
                // Consider tracking whether a channel
                // in non-empty, and only activating
                // on the basis of non-empty channels.
                let path = &self.paths.borrow_mut()[channel][..];
                // println!("Message for {:?}", path);
                self.activations
                    .borrow_mut()
                    .unpark(path);
            }
        }

        // Organize activations.
        self.activations
            .borrow_mut()
            .tidy();

        // println!("Active operators:");
        // for path in self.activations.borrow().active.iter() {
        //     println!("\t{:?}", path);
        // }

        // Step each dataflow once.
        self.dataflows
            .borrow_mut()
            .iter_mut()
            .for_each(|dataflow| { dataflow.step(); });

        // Flush logging infrastructure.
        self.logging.borrow_mut().flush();

        // Flush communication infrastructure.
        self.allocator.borrow_mut().release();

        // Discard completed dataflows, indicate non-emptiness.
        self.dataflows.borrow_mut().retain(|dataflow| dataflow.active());
        !self.dataflows.borrow().is_empty()
    }

    /// Calls `self.step()` as long as `func` evaluates to true.
    pub fn step_while<F: FnMut()->bool>(&mut self, mut func: F) {
        while func() { self.step(); }
    }

    /// The index of the worker out of its peers.
    pub fn index(&self) -> usize { self.allocator.borrow().index() }
    /// The total number of peer workers.
    pub fn peers(&self) -> usize { self.allocator.borrow().peers() }

    /// A timer started at the initiation of the timely computation.
    pub fn timer(&self) -> Instant { self.timer }

    /// Allocate a new worker-unique identifier.
    pub fn new_identifier(&mut self) -> usize {
        *self.identifiers.borrow_mut() += 1;
        *self.identifiers.borrow() - 1
    }

    /// Access to named loggers.
    pub fn log_register(&self) -> ::std::cell::RefMut<::logging_core::Registry<::logging::WorkerIdentifier>> {
        self.logging.borrow_mut()
    }

    /// Construct a new dataflow.
    pub fn dataflow<T: Timestamp, R, F:FnOnce(&mut Child<Self, T>)->R>(&mut self, func: F) -> R
    where
        T: Refines<()>
    {
        self.dataflow_using(Box::new(()), |_, child| func(child))
    }

    /// Construct a new dataflow binding resources that are released only after the dataflow is dropped.
    ///
    /// This method is designed to allow the dataflow builder to use certain resources that are then stashed
    /// with the dataflow until it has completed running. Once complete, the resources are dropped. The most
    /// common use of this method at present is with loading shared libraries, where the library is important
    /// for building the dataflow, and must be kept around until after the dataflow has completed operation.
    pub fn dataflow_using<T: Timestamp, R, F:FnOnce(&mut V, &mut Child<Self, T>)->R, V: Any+'static>(&mut self, mut resources: V, func: F) -> R
    where
        T: Refines<()>,
    {

        let addr = vec![self.allocator.borrow().index()];
        let dataflow_index = self.allocate_dataflow_index();

        let mut logging = self.logging.borrow_mut().get("timely");
        let subscope = SubgraphBuilder::new_from(dataflow_index, addr, logging.clone(), "Dataflow");
        let subscope = RefCell::new(subscope);

        let result = {
            let mut builder = Child {
                subgraph: &subscope,
                parent: self.clone(),
                logging: logging.clone(),
            };
            func(&mut resources, &mut builder)
        };

        logging.as_mut().map(|l| l.flush());

        let mut operator = subscope.into_inner().build(self);

        operator.get_internal_summary();
        operator.set_external_summary();

        let wrapper = Wrapper {
            _index: dataflow_index,
            operate: Some(Box::new(operator)),
            resources: Some(Box::new(resources)),
        };
        self.dataflows.borrow_mut().push(wrapper);

        result

    }

    // Acquire a new distinct dataflow identifier.
    fn allocate_dataflow_index(&mut self) -> usize {
        *self.dataflow_counter.borrow_mut() += 1;
        *self.dataflow_counter.borrow() - 1
    }
}

use communication::Message;

impl<A: Allocate> Clone for Worker<A> {
    fn clone(&self) -> Self {
        Worker {
            timer: self.timer,
            paths: self.paths.clone(),
            allocator: self.allocator.clone(),
            identifiers: self.identifiers.clone(),
            dataflows: self.dataflows.clone(),
            dataflow_counter: self.dataflow_counter.clone(),
            logging: self.logging.clone(),
            activations: self.activations.clone(),
        }
    }
}

struct Wrapper {
    _index: usize,
    operate: Option<Box<Schedule>>,
    resources: Option<Box<Any>>,
}

impl Wrapper {
    fn step(&mut self) -> bool {
        let active = self.operate.as_mut().map(|op| op.schedule()).unwrap_or(false);
        if !active {
            self.operate = None;
            self.resources = None;
        }
        // TODO consider flushing logs here (possibly after an arbitrary timeout)
        active
    }
    fn active(&self) -> bool { self.operate.is_some() }
}

impl Drop for Wrapper {
    fn drop(&mut self) {
        // println!("dropping dataflow {:?}", self._index);
        // ensure drop order
        self.operate = None;
        self.resources = None;
    }
}
