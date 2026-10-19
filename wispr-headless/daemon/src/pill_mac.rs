//! The flow bar on macOS (see bar.rs for what it shows).
//!
//! A borderless, non-activating panel above other windows on every Space,
//! drawn with Core Animation layers. The panel is exactly the size of the
//! drawn bar, so clicks next to it go through. AppKit runs on the main thread:
//! commands are forwarded to it with libdispatch, and a resting or hidden bar
//! schedules nothing. Sizes are points, which are Wispr's CSS pixels.
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
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::bar::{
    click_action, dot_alpha, BarState, Frame, Mode, PillCommand, BAR_GAP, BORDER, BOTTOM_MARGIN,
    DOT, DOT_STEP, FRAME,
};
use crate::wave;

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
        fn mouse_down(&self, _event: &NSEvent) {
            let mode = BAR.with(|b| b.try_borrow().ok().and_then(|b| b.as_ref().map(|b| b.mode)));
            if let (Some(action), Some(on_click)) = (mode.and_then(click_action), ON_CLICK.get()) {
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

fn color(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    NSColor::colorWithSRGBRed_green_blue_alpha(r, g, b, a)
}

fn rect(x: f64, y: f64, w: f64, h: f64) -> CGRect {
    CGRect::new(CGPoint::new(x, y), CGSize::new(w, h))
}

struct Bar {
    mtm: MainThreadMarker,
    panel: Retained<NSPanel>,
    body: Retained<CALayer>,
    bars: Vec<Retained<CALayer>>,
    dots: Vec<Retained<CALayer>>,
    state: BarState,
    /// The mode on screen.
    mode: Mode,
    /// A frame is scheduled.
    ticking: bool,
}

impl Bar {
    fn new(mtm: MainThreadMarker) -> Bar {
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
        let body = CALayer::new();
        body.setBorderWidth(BORDER);
        body.setBorderColor(Some(&color(0.667, 0.667, 0.667, 0.75).CGColor()));
        body.setBackgroundColor(Some(&color(0.0, 0.0, 0.0, 0.5).CGColor()));
        if let Some(root) = view.layer() {
            root.addSublayer(&body);
        }
        let white = NSColor::whiteColor().CGColor();
        let child = |radius: f64| {
            let l = CALayer::new();
            l.setBackgroundColor(Some(&white));
            l.setCornerRadius(radius);
            l.setHidden(true);
            body.addSublayer(&l);
            l
        };
        let bars = (0..wave::BARS).map(|_| child(wave::BAR_WIDTH / 2.0)).collect();
        let dots = (0..3).map(|_| child(DOT / 2.0)).collect();
        panel.setContentView(Some(&view));
        Bar {
            mtm,
            panel,
            body,
            bars,
            dots,
            state: BarState::default(),
            mode: Mode::Hidden,
            ticking: false,
        }
    }

    /// Bottom centre of the visible part (above the Dock) of the screen with
    /// the pointer, as Wispr places its bar on the display nearest the cursor.
    fn place(&self, mode: Mode) {
        let (w, h) = mode.size();
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
            self.layout(f.mode);
        }
        if f.mode.animated() {
            self.animate(&f);
            self.schedule();
        }
    }

    fn layout(&mut self, mode: Mode) {
        self.mode = mode;
        if mode == Mode::Hidden {
            self.panel.orderOut(None);
            return;
        }
        let (w, h) = mode.size();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        self.body.setFrame(rect(0.0, 0.0, w, h));
        self.body.setCornerRadius(h / 2.0);
        for b in &self.bars {
            b.setHidden(mode != Mode::Recording);
        }
        for d in &self.dots {
            d.setHidden(mode != Mode::Working);
        }
        CATransaction::commit();
        self.place(mode);
        self.panel.orderFrontRegardless();
    }

    fn animate(&self, f: &Frame) {
        let (w, h) = f.mode.size();
        CATransaction::begin();
        CATransaction::setDisableActions(true);
        match f.mode {
            Mode::Recording => {
                let bw = wave::BAR_WIDTH;
                let total = wave::BARS as f64 * bw + (wave::BARS as f64 - 1.0) * BAR_GAP;
                let left = (w - total) / 2.0;
                let opacity = if f.mic_active { 1.0 } else { 0.4 };
                for (i, (layer, &bh)) in self.bars.iter().zip(f.heights.iter()).enumerate() {
                    let bh = bh.max(bw).min(h - 4.0);
                    layer.setFrame(rect(left + i as f64 * (bw + BAR_GAP), (h - bh) / 2.0, bw, bh));
                    layer.setOpacity(opacity);
                }
            }
            Mode::Working => {
                for (i, layer) in self.dots.iter().enumerate() {
                    let cx = w / 2.0 + (i as f64 - 1.0) * DOT_STEP;
                    layer.setFrame(rect(cx - DOT / 2.0, (h - DOT) / 2.0, DOT, DOT));
                    layer.setOpacity(dot_alpha(f.t, i) as f32);
                }
            }
            Mode::Resting | Mode::Hidden => {}
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
/// the bar is shown (AppKit's run loop). `on_click` gets "start" or "stop"
/// when the bar is clicked.
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
    BAR.with(|b| *b.borrow_mut() = Some(Bar::new(mtm)));
    std::thread::spawn(move || {
        for command in commands {
            DispatchQueue::main().exec_async(move || with_bar(|bar| bar.apply(command)));
        }
    });
    app.run();
}
