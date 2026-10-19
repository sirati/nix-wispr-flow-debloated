//! The flow bar on macOS (see bar.rs for what it shows).
//!
//! A borderless, non-activating panel above other windows on every Space,
//! drawn with Core Animation layers, one per shape (y down: the root layer's
//! geometry is flipped). The panel is exactly the size of the pill, so clicks
//! next to it go through. AppKit runs on the main thread: commands are
//! forwarded to it with libdispatch, and a resting or hidden bar schedules
//! nothing. Sizes are points, which are Wispr's CSS pixels.
//!
//! Everything here is safe Rust through objc2's generated AppKit bindings.
//! The one class we define, `BarView`, overrides `mouseDown:` and
//! `acceptsFirstMouse:` and declares `initWithFrame:`; those selector
//! signatures (the `unsafe(...)` attributes) are checked against AppKit's
//! headers by hand.

use std::cell::RefCell;
use std::sync::mpsc::Receiver;
use std::sync::OnceLock;
use std::time::Instant;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::{Allocated, Retained};
use objc2::{define_class, extern_methods, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSPanel,
    NSScreen, NSStatusWindowLevel, NSView, NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_core_foundation::{CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::bar::{click_action, BarState, Frame, Mode, PillCommand, Shape, BOTTOM_MARGIN, FRAME};

type ClickFn = Box<dyn Fn(&'static str) + Send + Sync>;
static ON_CLICK: OnceLock<ClickFn> = OnceLock::new();

thread_local! {
    /// The bar, owned by the main thread.
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct BarView;

    impl BarView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = event.locationInWindow();
            let action = BAR.with(|b| {
                let b = b.try_borrow().ok()?;
                let bar = b.as_ref()?;
                let (_, h) = bar.mode.size();
                click_action(bar.mode, p.x, h - p.y)
            });
            if let (Some(action), Some(on_click)) = (action, ON_CLICK.get()) {
                on_click(action);
            }
        }

        // The panel never becomes key, so the first click has to count.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }
    }
);

impl BarView {
    extern_methods!(
        #[unsafe(method(initWithFrame:))]
        fn init_with_frame(this: Allocated<Self>, frame: NSRect) -> Retained<Self>;
    );
}

fn rgba([r, g, b, a]: [u8; 4]) -> Retained<NSColor> {
    let c = |v: u8| f64::from(v) / 255.0;
    NSColor::colorWithSRGBRed_green_blue_alpha(c(r), c(g), c(b), c(a))
}

/// A clockwise rotation as seen on screen. Core Animation turns a positive
/// angle counterclockwise even in our flipped (y-down) geometry, so the sign
/// is reversed (otherwise the check mark comes out mirrored).
fn rotation(angle: f64) -> CGAffineTransform {
    let (sin, cos) = (-angle).sin_cos();
    CGAffineTransform {
        a: cos,
        b: sin,
        c: -sin,
        d: cos,
        tx: 0.0,
        ty: 0.0,
    }
}

struct Bar {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    root: Retained<CALayer>,
    /// One layer per shape, reused from frame to frame.
    layers: Vec<Retained<CALayer>>,
    state: BarState,
    /// The mode on screen.
    mode: Mode,
    /// A frame is scheduled.
    ticking: bool,
}

impl Bar {
    fn new(mtm: MainThreadMarker) -> Option<Bar> {
        let (w, h) = Mode::Resting.size();
        let start = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, h));
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            start,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        // Never closed, only ordered out, so releasedWhenClosed does not matter.
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(false);
        panel.setLevel(NSStatusWindowLevel);
        panel.setFloatingPanel(true);
        panel.setHidesOnDeactivate(false);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        let view = BarView::init_with_frame(BarView::alloc(mtm), start);
        view.setWantsLayer(true);
        let root = view.layer()?;
        root.setGeometryFlipped(true);
        panel.setContentView(Some(&view));
        Some(Bar {
            mtm,
            panel,
            root,
            layers: Vec::new(),
            state: BarState::default(),
            mode: Mode::Hidden,
            ticking: false,
        })
    }

    /// Bottom centre of the visible part (above the Dock) of the screen with
    /// the pointer, as Wispr places its bar on the display nearest the cursor.
    fn place(&self, (w, h): (f64, f64)) {
        let mouse = NSEvent::mouseLocation();
        let screens = NSScreen::screens(self.mtm);
        let screen = screens
            .iter()
            .find(|s| {
                let f = s.frame();
                mouse.x >= f.origin.x
                    && mouse.x < f.origin.x + f.size.width
                    && mouse.y >= f.origin.y
                    && mouse.y < f.origin.y + f.size.height
            })
            .or_else(|| NSScreen::mainScreen(self.mtm));
        let Some(screen) = screen else { return };
        let v = screen.visibleFrame();
        let frame = NSRect::new(
            NSPoint::new(
                (v.origin.x + (v.size.width - w) / 2.0).round(),
                v.origin.y + BOTTOM_MARGIN,
            ),
            NSSize::new(w, h),
        );
        self.panel.setFrame_display(frame, false);
    }

    fn apply(&mut self, command: PillCommand) {
        self.state.apply(command);
        self.refresh();
    }

    fn refresh(&mut self) {
        let f = self.state.frame(Instant::now());
        if f.mode != self.mode {
            self.mode = f.mode;
            if f.mode == Mode::Hidden {
                self.panel.orderOut(None);
            } else {
                self.place(f.size);
                self.panel.orderFrontRegardless();
            }
        }
        if f.mode != Mode::Hidden {
            self.draw(&f);
        }
        if f.animated {
            self.schedule();
        }
    }

    fn draw(&mut self, f: &Frame) {
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        while self.layers.len() < f.shapes.len() {
            let layer = CALayer::new();
            self.root.addSublayer(&layer);
            self.layers.push(layer);
        }
        for (layer, s) in self.layers.iter().zip(&f.shapes) {
            show(layer, s);
        }
        for layer in &self.layers[f.shapes.len()..] {
            layer.setHidden(true);
        }
        CATransaction::commit();
    }

    /// One more frame while the bar is animated.
    fn schedule(&mut self) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        let when = DispatchTime::try_from(FRAME).unwrap_or(DispatchTime::NOW);
        let _ = DispatchQueue::main().after(when, || {
            with_bar(|bar| {
                bar.ticking = false;
                bar.refresh();
            })
        });
    }
}

fn show(layer: &CALayer, s: &Shape) {
    layer.setHidden(false);
    layer.setBounds(CGRect::new(CGPoint::new(0.0, 0.0), CGSize::new(s.w, s.h)));
    layer.setPosition(CGPoint::new(s.cx, s.cy));
    layer.setAffineTransform(rotation(s.angle));
    layer.setCornerRadius(s.radius);
    layer.setBackgroundColor(Some(&rgba(s.fill).CGColor()));
    match s.border {
        Some((color, width)) => {
            layer.setBorderWidth(width);
            layer.setBorderColor(Some(&rgba(color).CGColor()));
        }
        None => layer.setBorderWidth(0.0),
    }
}

fn with_bar(f: impl FnOnce(&mut Bar)) {
    BAR.with(|b| {
        if let Ok(mut b) = b.try_borrow_mut() {
            if let Some(bar) = b.as_mut() {
                f(bar);
            }
        }
    });
}

/// Runs the bar; must be called on the main thread, and does not return while
/// the bar is shown (AppKit's run loop). `on_click` gets "start", "stop" or
/// "cancel" when the bar is clicked.
pub fn run(
    commands: Receiver<PillCommand>,
    log: impl Fn(String),
    on_click: impl Fn(&'static str) + Send + Sync + 'static,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        log("flow bar off: not on the main thread".into());
        for _ in commands {}
        return;
    };
    let _ = ON_CLICK.set(Box::new(on_click));
    let app = NSApplication::sharedApplication(mtm);
    // No Dock icon and no menu bar: the bar is all there is.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let Some(bar) = Bar::new(mtm) else {
        log("flow bar off: no layer for the panel".into());
        for _ in commands {}
        return;
    };
    BAR.with(|b| *b.borrow_mut() = Some(bar));
    std::thread::spawn(move || {
        for command in commands {
            DispatchQueue::main().exec_async(move || with_bar(|bar| bar.apply(command)));
        }
    });
    app.run();
}
