//! Handle Windows viewer shutdown for both embedded callers and the standalone CLI.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use eframe::{egui, App, CreationContext, EframeWinitApplication, Frame, NativeOptions, UserEvent};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::run_on_demand::EventLoopExtRunOnDemand as _;
use winit::window::WindowId;

thread_local! {
    // Winit allows one EventLoop per process. Reuse it when a caller opens a second viewer.
    static EVENT_LOOP: RefCell<Option<EventLoop<UserEvent>>> = const { RefCell::new(None) };
}

pub(crate) fn run<A: App + 'static>(
    title: &str,
    options: NativeOptions,
    create: impl FnOnce(&CreationContext<'_>) -> A,
    exit_on_close: bool,
) -> eframe::Result {
    EVENT_LOOP.with(|slot| {
        let mut slot = slot
            .try_borrow_mut()
            .map_err(|error| eframe::Error::AppCreation(Box::new(error)))?;
        let event_loop = match slot.as_mut() {
            Some(event_loop) => event_loop,
            None => slot.insert(EventLoop::with_user_event().build()?),
        };
        let closed = Rc::new(Cell::new(false));
        let started = Rc::new(Cell::new(false));
        let app_closed = Rc::clone(&closed);
        let app_started = Rc::clone(&started);
        let inner = eframe::create_native(
            title,
            options,
            Box::new(move |context| {
                let app = create(context);
                app_started.set(true);
                Ok(Box::new(ExitSignal {
                    app,
                    closed: app_closed,
                }))
            }),
            event_loop,
        );
        let mut app = WindowsViewer { inner, closed, exit_on_close };
        event_loop.run_app_on_demand(&mut app)?;
        if started.get() {
            Ok(())
        } else {
            Err(eframe::Error::AppCreation(
                "Unable to initialize the native viewer".into(),
            ))
        }
    })
}

struct ExitSignal<A> {
    app: A,
    closed: Rc<Cell<bool>>,
}

impl<A: App> App for ExitSignal<A> {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut Frame) {
        self.app.ui(ui, frame);
    }

    fn logic(&mut self, ctx: &egui::Context, frame: &mut Frame) {
        self.app.logic(ctx, frame);
    }

    fn on_exit(&mut self) {
        self.app.on_exit();
        self.closed.set(true);
    }
}

struct WindowsViewer<'a> {
    inner: EframeWinitApplication<'a>,
    closed: Rc<Cell<bool>>,
    exit_on_close: bool,
}

impl WindowsViewer<'_> {
    fn exit_if_closed(&self, event_loop: &ActiveEventLoop) {
        // Eframe 0.37 drops the app on close and relies on a subsequent native
        // window event to exit. That event can be delayed while a handle survives.
        if self.closed.get() {
            event_loop.exit();
        }
    }
}

impl ApplicationHandler<UserEvent> for WindowsViewer<'_> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.resumed(event_loop);
        self.exit_if_closed(event_loop);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if self.exit_on_close && matches!(event, WindowEvent::CloseRequested) {
            // The viewer has a single native window. Exit before eframe starts tearing it down.
            terminate_cli();
        }
        self.inner.window_event(event_loop, id, event);
        self.exit_if_closed(event_loop);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        self.inner.new_events(event_loop, cause);
        self.exit_if_closed(event_loop);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        self.inner.user_event(event_loop, event);
        self.exit_if_closed(event_loop);
    }

    fn device_event(&mut self, event_loop: &ActiveEventLoop, id: DeviceId, event: DeviceEvent) {
        self.inner.device_event(event_loop, id, event);
        self.exit_if_closed(event_loop);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.about_to_wait(event_loop);
        self.exit_if_closed(event_loop);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.suspended(event_loop);
    }

    fn exiting(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.exiting(event_loop);
    }

    fn memory_warning(&mut self, event_loop: &ActiveEventLoop) {
        self.inner.memory_warning(event_loop);
    }
}

fn terminate_cli() -> ! {
    use std::ffi::c_void;

    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn TerminateProcess(process: *mut c_void, exit_code: u32) -> i32;
    }

    // SAFETY: GetCurrentProcess returns a valid pseudo-handle for this process. Terminating it
    // skips DLL detach callbacks, including graphics-driver code that can lock during shutdown.
    unsafe { TerminateProcess(GetCurrentProcess(), 0) };
    std::process::exit(0);
}
