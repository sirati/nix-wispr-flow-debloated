//! The flow bar on macOS (see bar.rs for what it shows).
//!
//! A borderless, non-activating panel above other windows on every Space,
//! drawn with Core Animation layers, one per shape (y down: they sit in a
//! flipped canvas layer of our own; AppKit draws a view's own backing layer
//! y up even when it is marked flipped), and AppKit labels for text in the
//! system font. The panel is exactly the size of the pill, so clicks next to
//! it go through. AppKit runs on the main thread: commands are forwarded to
//! it with libdispatch, and a resting or hidden bar schedules nothing. Sizes
//! are points, which are Wispr's CSS pixels.
//!
//! Everything here is safe Rust through objc2's generated AppKit bindings.
//! The one class we define, `BarView`, overrides `mouseDown:`,
//! `acceptsFirstMouse:` and `hitTest:` and declares `initWithFrame:`; those
//! selector signatures (the `unsafe(...)` attributes) are checked against
//! AppKit's headers by hand.

use std::cell::RefCell;
use std::sync::mpsc::Receiver;
use std::sync::OnceLock;
use std::time::Instant;

use dispatch2::{DispatchQueue, DispatchTime};
use objc2::rc::{Allocated, Retained};
use objc2::{define_class, extern_methods, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSEvent, NSFont,
    NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSView, NSWindowCollectionBehavior,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGAffineTransform, CGPoint, CGRect, CGSize};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
use objc2_quartz_core::{CALayer, CATransaction};

use crate::bar::{BarEvent, BarState, Frame, Mode, PillCommand, Shape, Text, BOTTOM_MARGIN, FRAME};

type EventFn = Box<dyn Fn(BarEvent) + Send + Sync>;
static ON_EVENT: OnceLock<EventFn> = OnceLock::new();

thread_local! {
    /// The bar, owned by the main thread.
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
}

fn emit(e: BarEvent) {
    if let Some(on_event) = ON_EVENT.get() {
        on_event(e);
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    struct BarView;

    impl BarView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let p = event.locationInWindow();
            with_bar(|bar| {
                if let Some(e) = bar.state.click(p.x, bar.size.1 - p.y) {
                    emit(e);
                }
                bar.refresh();
            });
        }

        // The panel never becomes key, so the first click has to count.
        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _event: Option<&NSEvent>) -> bool {
            true
        }

        // Clicks on the text labels are ours too.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            (self as *const Self).cast_mut().cast()
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

/// The system font; semibold stands in for Wispr's Figtree 550.
fn font(size: f64, bold: bool) -> Retained<NSFont> {
    NSFont::systemFontOfSize_weight(size, if bold { 0.3 } else { 0.0 })
}

/// A text field adds this much around its text.
const LABEL_INSET: f64 = 2.0;

/// A clockwise rotation as seen on screen: in the flipped canvas a positive
/// angle turns clockwise, as in our y-down shapes.
fn rotation(angle: f64) -> CGAffineTransform {
    let (sin, cos) = angle.sin_cos();
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
    view: Retained<BarView>,
    /// Holds the shape layers, y down.
    canvas: Retained<CALayer>,
    /// One layer per shape and one label per text, reused between frames.
    layers: Vec<Retained<CALayer>>,
    labels: Vec<Retained<NSTextField>>,
    /// Measures text widths.
    measurer: Retained<NSTextField>,
    state: BarState,
    /// The mode and pill size on screen.
    mode: Mode,
    size: (f64, f64),
    /// A frame is scheduled.
    ticking: bool,
}

impl Bar {
    fn new(mtm: MainThreadMarker) -> Option<Bar> {
        let start = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(40.0, 8.0));
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
        let canvas = CALayer::new();
        canvas.setGeometryFlipped(true);
        view.layer()?.addSublayer(&canvas);
        panel.setContentView(Some(&view));
        Some(Bar {
            mtm,
            panel,
            view,
            canvas,
            layers: Vec::new(),
            labels: Vec::new(),
            measurer: NSTextField::labelWithString(&NSString::from_str(""), mtm),
            state: BarState::default(),
            mode: Mode::Hidden,
            size: (0.0, 0.0),
            ticking: false,
        })
    }

    /// The screen with the pointer, as Wispr places its bar on the display
    /// nearest the cursor.
    fn screen(&self) -> Option<Retained<NSScreen>> {
        let mouse = NSEvent::mouseLocation();
        NSScreen::screens(self.mtm)
            .iter()
            .find(|s| inside(mouse, s.frame()))
            .or_else(|| NSScreen::mainScreen(self.mtm))
    }

    /// Bottom centre of the visible part of the screen (above the Dock).
    fn place(&self, (w, h): (f64, f64)) {
        let Some(screen) = self.screen() else { return };
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

    /// The pointer in the pill (y down), if it is on it.
    fn pointer(&self) -> Option<(f64, f64)> {
        let mouse = NSEvent::mouseLocation();
        let f = self.panel.frame();
        inside(mouse, f).then(|| (mouse.x - f.origin.x, f.size.height - (mouse.y - f.origin.y)))
    }

    fn apply(&mut self, command: PillCommand) {
        self.state.apply(command);
        self.refresh();
    }

    fn refresh(&mut self) {
        let pointer = if self.mode == Mode::Notice {
            self.pointer()
        } else {
            None
        };
        let measurer = &self.measurer;
        let measure = |text: &str, size: f64, bold: bool| {
            measurer.setFont(Some(&font(size, bold)));
            measurer.setStringValue(&NSString::from_str(text));
            measurer.intrinsicContentSize().width - 2.0 * LABEL_INSET
        };
        let f = self.state.frame(Instant::now(), &measure, pointer);
        for e in &f.events {
            emit(*e);
        }
        if f.mode != self.mode || f.size != self.size {
            self.mode = f.mode;
            self.size = f.size;
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
        self.canvas.setFrame(CGRect::new(
            CGPoint::new(0.0, 0.0),
            CGSize::new(f.size.0, f.size.1),
        ));
        while self.layers.len() < f.shapes.len() {
            let layer = CALayer::new();
            self.canvas.addSublayer(&layer);
            self.layers.push(layer);
        }
        for (layer, s) in self.layers.iter().zip(&f.shapes) {
            show(layer, s);
        }
        for layer in &self.layers[f.shapes.len()..] {
            layer.setHidden(true);
        }
        CATransaction::commit();

        while self.labels.len() < f.texts.len() {
            let label = NSTextField::labelWithString(&NSString::from_str(""), self.mtm);
            self.view.addSubview(&label);
            self.labels.push(label);
        }
        for (label, t) in self.labels.iter().zip(&f.texts) {
            self.show_text(label, t, f.size.1);
        }
        for label in &self.labels[f.texts.len()..] {
            label.setHidden(true);
        }
    }

    fn show_text(&self, label: &NSTextField, t: &Text, height: f64) {
        label.setHidden(false);
        label.setFont(Some(&font(t.size, t.bold)));
        label.setTextColor(Some(&rgba(t.color)));
        label.setStringValue(&NSString::from_str(&t.text));
        let size = label.intrinsicContentSize();
        // The view's own coordinates are y up.
        let y = height - t.top - (t.line + size.height) / 2.0;
        label.setFrame(NSRect::new(
            NSPoint::new(t.x - LABEL_INSET, y),
            NSSize::new(size.width, size.height),
        ));
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

fn inside(p: NSPoint, r: NSRect) -> bool {
    p.x >= r.origin.x
        && p.x < r.origin.x + r.size.width
        && p.y >= r.origin.y
        && p.y < r.origin.y + r.size.height
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
/// the bar is shown (AppKit's run loop). `on_event` gets clicks and
/// notification results. `_fonts` is for Linux; text uses the system font.
pub fn run(
    commands: Receiver<PillCommand>,
    _fonts: Option<&str>,
    log: impl Fn(String),
    on_event: impl Fn(BarEvent) + Send + Sync + 'static,
) {
    let Some(mtm) = MainThreadMarker::new() else {
        log("flow bar off: not on the main thread".into());
        for _ in commands {}
        return;
    };
    let _ = ON_EVENT.set(Box::new(on_event));
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
