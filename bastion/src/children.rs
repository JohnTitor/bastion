use crate::broadcast::{Broadcast, Parent, Sender};
use crate::context::{BastionContext, BastionId, ContextState};
use crate::message::{Answer, BastionMessage, Message};
use crate::system::schedule;
use futures::pending;
use futures::poll;
use futures::prelude::*;
use futures::stream::{FuturesOrdered, FuturesUnordered};
use fxhash::FxHashMap;
use lightproc::prelude::*;
use qutex::Qutex;
use std::fmt::{self, Debug, Formatter};
use std::future::Future;
use std::iter::FromIterator;
use std::pin::Pin;
use std::task::{Context, Poll};

struct Init(Box<dyn Fn(BastionContext) -> Exec + Send + Sync>);
struct Exec(Pin<Box<dyn Future<Output = Result<(), ()>> + Send>>);

#[derive(Debug)]
/// A children group that will contain a defined number of
/// elements (set with [`with_redundancy`] or `1` by default)
/// all running a future (returned by the closure that is set
/// with [`with_exec`]).
///
/// When an element of the group stops or panics, all the
/// elements will be stopped as well and the group's supervisor
/// will receive a notice that a stop or panic occurred (note
/// that if a panic occurred, the supervisor will restart the
/// children group and eventually some of its other children,
/// depending on its [`SupervisionStrategy`]).
///
/// # Example
///
/// ```
/// # use bastion::prelude::*;
/// #
/// # fn main() {
///     # Bastion::init();
///     #
/// let children_ref: ChildrenRef = Bastion::children(|children| {
///     // Configure the children group...
///     children.with_exec(|ctx: BastionContext| {
///         async move {
///             // Send and receive messages...
///             let opt_msg: Option<Msg> = ctx.try_recv().await;
///             // ...and return `Ok(())` or `Err(())` when you are done...
///             Ok(())
///
///             // Note that if `Err(())` was returned, the supervisor would
///             // restart the children group.
///         }
///     })
///     // ...and return it.
/// }).expect("Couldn't create the children group.");
///     #
///     # Bastion::start();
///     # Bastion::stop();
///     # Bastion::block_until_stopped();
/// # }
/// ```
///
/// [`with_redundancy`]: #method.with_redundancy
/// [`with_exec`]: #method.with_exec
/// [`SupervisionStrategy`]: supervisor/enum.SupervisionStrategy.html
pub struct Children {
    bcast: Broadcast,
    // The currently launched elements of the group.
    launched: FxHashMap<BastionId, (Sender, RecoverableHandle<()>)>,
    // The closure returning the future that will be used by
    // every element of the group.
    init: Init,
    redundancy: usize,
    // Messages that were received before the group was
    // started. Those will be "replayed" once a start message
    // is received.
    pre_start_msgs: Vec<BastionMessage>,
    started: bool,
}

#[derive(Debug, Clone)]
/// A "reference" to a children group, allowing to communicate
/// with it.
pub struct ChildrenRef {
    id: BastionId,
    sender: Sender,
    children: Vec<ChildRef>,
}

#[derive(Debug)]
pub(crate) struct Child {
    bcast: Broadcast,
    // The future that this child is executing.
    exec: Exec,
    // A lock behind which is the child's context state.
    // This is used to store the messages that were received
    // for the child's associated future to be able to
    // retrieve them.
    state: Qutex<ContextState>,
    // Messages that were received before the child was
    // started. Those will be "replayed" once a start message
    // is received.
    pre_start_msgs: Vec<BastionMessage>,
    started: bool,
}

#[derive(Debug, Clone)]
/// A "reference" to an element of a children group, allowing to
/// communicate with it.
pub struct ChildRef {
    id: BastionId,
    sender: Sender,
}

impl Init {
    fn new<C, F>(init: C) -> Self
    where
        C: Fn(BastionContext) -> F + Send + Sync + 'static,
        F: Future<Output = Result<(), ()>> + Send + 'static,
    {
        let init = Box::new(move |ctx: BastionContext| {
            let fut = init(ctx);
            let exec = Box::pin(fut);

            Exec(exec)
        });

        Init(init)
    }
}

impl Children {
    pub(crate) fn new(bcast: Broadcast) -> Self {
        let launched = FxHashMap::default();
        let init = Init::default();
        let redundancy = 1;
        let pre_start_msgs = Vec::new();
        let started = false;

        Children {
            bcast,
            launched,
            init,
            redundancy,
            pre_start_msgs,
            started,
        }
    }

    fn stack(&self) -> ProcStack {
        // FIXME: with_pid
        ProcStack::default()
    }

    pub(crate) async fn reset(&mut self, bcast: Broadcast) {
        // TODO: stop or kill?
        self.kill().await;

        self.bcast = bcast;

        self.launch_elems();
    }

    pub(crate) fn id(&self) -> &BastionId {
        self.bcast.id()
    }

    pub(crate) fn bcast(&self) -> &Broadcast {
        &self.bcast
    }

    pub(crate) fn as_ref(&self) -> ChildrenRef {
        // TODO: clone or ref?
        let id = self.bcast.id().clone();
        let sender = self.bcast.sender().clone();

        let mut children = Vec::with_capacity(self.launched.len());
        for (id, (sender, _)) in &self.launched {
            // TODO: clone or ref?
            let child = ChildRef::new(id.clone(), sender.clone());
            children.push(child);
        }

        ChildrenRef::new(id, sender, children)
    }

    /// Sets the closure taking a [`BastionContext`] and returning a
    /// [`Future`] that will be used by every element of this children
    /// group.
    ///
    /// When a new element is started, it will be assigned a new context,
    /// pass it to the `init` closure and poll the returned future until
    /// it stops, panics or another element of the group stops or panics.
    ///
    /// The returned future's output should be `Result<(), ()>`.
    ///
    /// # Arguments
    ///
    /// * `init` - The closure taking a [`BastionContext`] and returning
    ///     a [`Future`] that will be used by every element of this
    ///     children group.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    /// Bastion::children(|children| {
    ///     children.with_exec(|ctx| {
    ///         async move {
    ///             // Send and receive messages...
    ///             let opt_msg: Option<Msg> = ctx.try_recv().await;
    ///             // ...and return `Ok(())` or `Err(())` when you are done...
    ///             Ok(())
    ///
    ///             // Note that if `Err(())` was returned, the supervisor would
    ///             // restart the children group.
    ///         }
    ///     })
    /// }).expect("Couldn't create the children group.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn with_exec<I, F>(mut self, init: I) -> Self
    where
        I: Fn(BastionContext) -> F + Send + Sync + 'static,
        F: Future<Output = Result<(), ()>> + Send + 'static,
    {
        self.init = Init::new(init);
        self
    }

    /// Sets the number of number of elements this children group will
    /// contain. Each element will call the closure passed in
    /// [`with_exec`] and run the returned future until it stops,
    /// panics or another element in the group stops or panics.
    ///
    /// The default number of elements a children group contains is `1`.
    ///
    /// # Arguments
    ///
    /// * `redundancy` - The number of elements this group will contain.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    /// Bastion::children(|children| {
    ///     // Note that "1" is the default number of elements.
    ///     children.with_redundancy(1)
    /// }).expect("Couldn't create the children group.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn with_redundancy(mut self, redundancy: usize) -> Self {
        self.redundancy = redundancy;
        self
    }

    async fn stop(&mut self) {
        self.bcast.stop_children();

        let launched = self.launched.drain().map(|(_, (_, launched))| launched);
        FuturesUnordered::from_iter(launched)
            .for_each_concurrent(None, |_| async {})
            .await;
    }

    async fn kill(&mut self) {
        self.bcast.kill_children();

        let mut children = FuturesOrdered::new();
        for (_, (_, launched)) in self.launched.drain() {
            launched.cancel();

            children.push(launched);
        }

        children.for_each_concurrent(None, |_| async {}).await;
    }

    fn stopped(&mut self) {
        self.bcast.stopped();
    }

    fn faulted(&mut self) {
        self.bcast.faulted();
    }

    async fn handle(&mut self, msg: BastionMessage) -> Result<(), ()> {
        match msg {
            BastionMessage::Start => unreachable!(),
            BastionMessage::Stop => {
                self.stop().await;
                self.stopped();

                return Err(());
            }
            BastionMessage::Kill => {
                self.kill().await;
                self.stopped();

                return Err(());
            }
            // FIXME
            BastionMessage::Deploy(_) => unimplemented!(),
            // FIXME
            BastionMessage::Prune { .. } => unimplemented!(),
            // FIXME
            BastionMessage::SuperviseWith(_) => unimplemented!(),
            BastionMessage::Message { .. } => {
                self.bcast.send_children(msg);
            }
            BastionMessage::Stopped { id } => {
                // FIXME: Err if false?
                if self.launched.contains_key(&id) {
                    self.stop().await;
                    self.stopped();

                    return Err(());
                }
            }
            BastionMessage::Faulted { id } => {
                // FIXME: Err if false?
                if self.launched.contains_key(&id) {
                    self.kill().await;
                    self.faulted();

                    return Err(());
                }
            }
        }

        Ok(())
    }

    async fn run(mut self) -> Self {
        loop {
            match poll!(&mut self.bcast.next()) {
                // TODO: Err if started == true?
                Poll::Ready(Some(BastionMessage::Start)) => {
                    self.started = true;

                    let msg = BastionMessage::start();
                    self.bcast.send_children(msg);

                    let msgs = self.pre_start_msgs.drain(..).collect::<Vec<_>>();
                    self.pre_start_msgs.shrink_to_fit();

                    for msg in msgs {
                        if self.handle(msg).await.is_err() {
                            return self;
                        }
                    }
                }
                Poll::Ready(Some(msg)) if !self.started => {
                    self.pre_start_msgs.push(msg);
                }
                Poll::Ready(Some(msg)) => {
                    if self.handle(msg).await.is_err() {
                        return self;
                    }
                }
                Poll::Ready(None) => {
                    // TODO: stop or kill?
                    self.kill().await;
                    self.faulted();

                    return self;
                }
                Poll::Pending => (),
            }

            for (_, launched) in self.launched.values_mut() {
                let _ = poll!(launched);
            }
        }
    }

    pub(crate) fn launch_elems(&mut self) {
        for _ in 0..self.redundancy {
            let parent = Parent::children(self.as_ref());
            let bcast = Broadcast::new(parent);
            // TODO: clone or ref?
            let id = bcast.id().clone();
            let sender = bcast.sender().clone();

            let child_ref = ChildRef::new(id.clone(), sender.clone());
            let children = self.as_ref();
            // FIXME
            let supervisor = self.bcast.parent().clone().into_supervisor().unwrap();

            let state = ContextState::new();
            let state = Qutex::new(state);

            let ctx =
                BastionContext::new(id.clone(), child_ref, children, supervisor, state.clone());
            let exec = (self.init.0)(ctx);

            self.bcast.register(&bcast);

            let child = Child::new(exec, bcast, state);
            let launched = child.launch();

            self.launched.insert(id.clone(), (sender, launched));
        }
    }

    pub(crate) fn launch(self) -> ProcHandle<Self> {
        let stack = self.stack();
        let (proc, handle) = LightProc::build(self.run(), schedule, stack);

        proc.schedule();
        handle
    }
}

impl ChildrenRef {
    fn new(id: BastionId, sender: Sender, children: Vec<ChildRef>) -> Self {
        ChildrenRef {
            id,
            sender,
            children,
        }
    }

    /// Returns a list of [`ChildRef`] referencing the elements
    /// of the children group this `ChildrenRef` is referencing.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    /// let elems: &[ChildRef] = children_ref.elems();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`ChildRef`]: children/struct.ChildRef.html
    pub fn elems(&self) -> &[ChildRef] {
        &self.children
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing which will then send it to all of its
    /// elements.
    ///
    /// An alternative would be to use [`elems`] to get all the
    /// elements of the group and then send the message to all
    /// of them.
    ///
    /// This method returns `()` if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Arguments
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    /// let msg = "A message containing data.";
    /// children_ref.broadcast(msg).expect("Couldn't send the message.");
    ///
    ///     # Bastion::children(|children| {
    ///         # children.with_exec(|ctx: BastionContext| {
    ///             # async move {
    /// // And then in every of the children group's elements' futures...
    /// msg! { ctx.recv().await?,
    ///     ref msg: &'static str => {
    ///         assert_eq!(msg, &"A message containing data.");
    ///     };
    ///     // We are only sending a `&'static str` in this
    ///     // example, so we know that this won't happen...
    ///     _: _ => ();
    /// }
    ///                 #
    ///                 # Ok(())
    ///             # }
    ///         # })
    ///     # }).unwrap();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`elems`]: #method.elems
    pub fn broadcast<M: Message>(&self, msg: M) -> Result<(), M> {
        let msg = BastionMessage::broadcast(msg);
        // FIXME: panics?
        self.send(msg).map_err(|err| err.into_msg().unwrap())
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing to tell it to stop all of its running
    /// elements.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    /// children_ref.stop().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn stop(&self) -> Result<(), ()> {
        let msg = BastionMessage::stop();
        self.send(msg).map_err(|_| ())
    }

    /// Sends a message to the children group this `ChildrenRef`
    /// is referencing to tell it to kill all of its running
    /// elements.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    /// children_ref.kill().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn kill(&self) -> Result<(), ()> {
        let msg = BastionMessage::kill();
        self.send(msg).map_err(|_| ())
    }

    pub(crate) fn send(&self, msg: BastionMessage) -> Result<(), BastionMessage> {
        self.sender
            .unbounded_send(msg)
            .map_err(|err| err.into_inner())
    }
}

impl Child {
    fn new(exec: Exec, bcast: Broadcast, state: Qutex<ContextState>) -> Self {
        let pre_start_msgs = Vec::new();
        let started = false;

        Child {
            bcast,
            exec,
            state,
            pre_start_msgs,
            started,
        }
    }

    fn stack(&self) -> ProcStack {
        let id = self.bcast.id().clone();
        // FIXME: panics?
        let parent = self.bcast.parent().clone().into_children().unwrap();

        // FIXME: with_pid
        ProcStack::default().with_after_panic(move || {
            // FIXME: clones
            let id = id.clone();

            let msg = BastionMessage::faulted(id);
            // TODO: handle errors
            parent.send(msg).ok();
        })
    }

    fn stopped(&mut self) {
        self.bcast.stopped();
    }

    fn faulted(&mut self) {
        self.bcast.faulted();
    }

    async fn handle(&mut self, msg: BastionMessage) -> Result<(), ()> {
        match msg {
            BastionMessage::Start => unreachable!(),
            BastionMessage::Stop | BastionMessage::Kill => {
                self.stopped();

                return Err(());
            }
            // FIXME
            BastionMessage::Deploy(_) => unimplemented!(),
            // FIXME
            BastionMessage::Prune { .. } => unimplemented!(),
            // FIXME
            BastionMessage::SuperviseWith(_) => unimplemented!(),
            BastionMessage::Message(msg) => {
                let mut state = self.state.clone().lock_async().await.map_err(|_| ())?;
                state.push_msg(msg);
            }
            // FIXME
            BastionMessage::Stopped { .. } => unimplemented!(),
            // FIXME
            BastionMessage::Faulted { .. } => unimplemented!(),
        }

        Ok(())
    }

    async fn run(mut self) {
        loop {
            match poll!(&mut self.bcast.next()) {
                // TODO: Err if started == true?
                Poll::Ready(Some(BastionMessage::Start)) => {
                    self.started = true;

                    let msgs = self.pre_start_msgs.drain(..).collect::<Vec<_>>();
                    self.pre_start_msgs.shrink_to_fit();

                    for msg in msgs {
                        if self.handle(msg).await.is_err() {
                            return;
                        }
                    }

                    continue;
                }
                Poll::Ready(Some(msg)) if !self.started => {
                    self.pre_start_msgs.push(msg);

                    continue;
                }
                Poll::Ready(Some(msg)) => {
                    if self.handle(msg).await.is_err() {
                        return;
                    }

                    continue;
                }
                Poll::Ready(None) => {
                    self.faulted();

                    return;
                }
                Poll::Pending => (),
            }

            if !self.started {
                pending!();

                continue;
            }

            match poll!(&mut self.exec) {
                Poll::Ready(Ok(())) => return self.stopped(),
                Poll::Ready(Err(())) => return self.faulted(),
                Poll::Pending => (),
            }

            pending!();
        }
    }

    fn launch(self) -> RecoverableHandle<()> {
        let stack = self.stack();
        let (proc, handle) = LightProc::recoverable(self.run(), schedule, stack);

        proc.schedule();
        handle
    }
}

impl ChildRef {
    fn new(id: BastionId, sender: Sender) -> ChildRef {
        ChildRef { id, sender }
    }

    /// Sends a message to the child this `ChildRef` is referencing.
    ///
    /// This method returns `()` if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Argument
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    /// // The message that will be "told"...
    /// const TELL_MSG: &'static str = "A message containing data (tell).";
    ///
    ///     # let children_ref =
    /// // Create a new child...
    /// Bastion::children(|children| {
    ///     children.with_exec(|ctx: BastionContext| {
    ///         async move {
    ///             // ...which will receive the message "told"...
    ///             msg! { ctx.recv().await?,
    ///                 msg: &'static str => {
    ///                     assert_eq!(msg, TELL_MSG);
    ///                     // Handle the message...
    ///                 };
    ///                 // This won't happen because this example
    ///                 // only "tells" a `&'static str`...
    ///                 _: _ => ();
    ///             }
    ///
    ///             Ok(())
    ///         }
    ///     })
    /// }).expect("Couldn't create the children group.");
    ///
    ///     # let child_ref = &children_ref.elems()[0];
    /// // Later, the message is "told" to the child...
    /// child_ref.tell(TELL_MSG).expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn tell<M: Message>(&self, msg: M) -> Result<(), M> {
        let msg = BastionMessage::tell(msg);
        // FIXME: panics?
        self.send(msg).map_err(|msg| msg.into_msg().unwrap())
    }

    /// Sends a message to the child this `ChildRef` is referencing,
    /// allowing it to answer.
    ///
    /// This method returns [`Answer`] if it succeeded, or `Err(msg)`
    /// otherwise.
    ///
    /// # Argument
    ///
    /// * `msg` - The message to send.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    /// // The message that will be "asked"...
    /// const ASK_MSG: &'static str = "A message containing data (ask).";
    /// // The message the will be "answered"...
    /// const ANSWER_MSG: &'static str = "A message containing data (answer).";
    ///
    ///     # let children_ref =
    /// // Create a new child...
    /// Bastion::children(|children| {
    ///     children.with_exec(|ctx: BastionContext| {
    ///         async move {
    ///             // ...which will receive the message asked...
    ///             msg! { ctx.recv().await?,
    ///                 msg: &'static str =!> {
    ///                     assert_eq!(msg, ASK_MSG);
    ///                     // Handle the message...
    ///
    ///                     // ...and eventually answer to it...
    ///                     answer!(ANSWER_MSG);
    ///                 };
    ///                 // This won't happen because this example
    ///                 // only "asks" a `&'static str`...
    ///                 _: _ => ();
    ///             }
    ///
    ///             Ok(())
    ///         }
    ///     })
    /// }).expect("Couldn't create the children group.");
    ///
    ///     # Bastion::children(|children| {
    ///         # children.with_exec(move |ctx: BastionContext| {
    ///             # let child_ref = children_ref.elems()[0].clone();
    ///             # async move {
    /// // Later, the message is "asked" to the child...
    /// let answer: Answer = child_ref.ask(ASK_MSG).expect("Couldn't send the message.");
    ///
    /// // ...and the child's answer is received...
    /// msg! { answer.await.expect("Couldn't receive the answer."),
    ///     msg: &'static str => {
    ///         assert_eq!(msg, ANSWER_MSG);
    ///         // Handle the answer...
    ///     };
    ///     // This won't happen because this example
    ///     // only answers a `&'static str`...
    ///     _: _ => ();
    /// }
    ///                 #
    ///                 # Ok(())
    ///             # }
    ///         # })
    ///     # }).unwrap();
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    ///
    /// [`Answer`]: message/struct.Answer.html
    pub fn ask<M: Message>(&self, msg: M) -> Result<Answer, M> {
        let (msg, answer) = BastionMessage::ask(msg);
        // FIXME: panics?
        self.send(msg).map_err(|msg| msg.into_msg().unwrap())?;

        Ok(answer)
    }

    /// Sends a message to the child this `ChildRef` is referencing
    /// to tell it to stop its execution.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    ///     # let child_ref = &children_ref.elems()[0];
    /// child_ref.stop().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn stop(&self) -> Result<(), ()> {
        let msg = BastionMessage::stop();
        self.send(msg).map_err(|_| ())
    }

    /// Sends a message to the child this `ChildRef` is referencing
    /// to tell it to suicide.
    ///
    /// This method returns `()` if it succeeded, or `Err(())`
    /// otherwise.
    ///
    /// # Example
    ///
    /// ```
    /// # use bastion::prelude::*;
    /// #
    /// # fn main() {
    ///     # Bastion::init();
    ///     #
    ///     # let children_ref = Bastion::children(|children| children).unwrap();
    ///     # let child_ref = &children_ref.elems()[0];
    /// child_ref.kill().expect("Couldn't send the message.");
    ///     #
    ///     # Bastion::start();
    ///     # Bastion::stop();
    ///     # Bastion::block_until_stopped();
    /// # }
    /// ```
    pub fn kill(&self) -> Result<(), ()> {
        let msg = BastionMessage::kill();
        self.send(msg).map_err(|_| ())
    }

    pub(crate) fn send(&self, msg: BastionMessage) -> Result<(), BastionMessage> {
        self.sender
            .unbounded_send(msg)
            .map_err(|err| err.into_inner())
    }
}

impl Future for Exec {
    type Output = Result<(), ()>;

    fn poll(self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().0).poll(ctx)
    }
}

impl Default for Init {
    fn default() -> Self {
        Init::new(|_| async { Ok(()) })
    }
}

impl Debug for Init {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Init").finish()
    }
}

impl Debug for Exec {
    fn fmt(&self, fmt: &mut Formatter) -> fmt::Result {
        fmt.debug_struct("Exec").finish()
    }
}