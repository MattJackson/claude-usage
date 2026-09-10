//! Native-AppKit `NSPopover` renderer for the [`PopoverModel`], anchored to the
//! `NSStatusItem` button (Phase 1 of `docs/design/custom-tray-popup.md`).
//!
//! Why this instead of the native `NSMenu`: an `NSMenu` reserves a
//! disclosure-chevron column at its right edge, so our right-aligned
//! `S% / W%` trailing never sits truly flush — the maintainer's #1 visual
//! issue. Here we draw the rows ourselves into a flipped `NSView`, so the
//! trailing percentages right-align to a single column at `WIDTH - H_PAD` with
//! NO chevron and NO reserved gap.
//!
//! Robustness: `NSPopover` with `.transient` behavior dismisses on
//! click-outside / focus loss for free, computes the on-screen anchor (incl.
//! which display the menu bar is on) for free, and follows dark/light mode and
//! HiDPI automatically because every color is a semantic `NSColor`
//! (`labelColor`, `secondaryLabelColor`, `controlAccentColor`, `separatorColor`,
//! `systemOrange/RedColor`) and every size is in points.
//!
//! Clicks reuse the existing id-routing: each row view stores the same click-id
//! string the native menu carried, and on `mouseDown:` hands it to the
//! `on_click` callback `menubar::run` wired to `handle_click`. No
//! switch/launch/remove logic is duplicated here.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{define_class, msg_send, AnyThread, DefinedClass, MainThreadMarker};
use objc2_app_kit::{
    NSBox, NSBoxType, NSColor, NSEvent, NSFont, NSFontAttributeName,
    NSForegroundColorAttributeName, NSImage, NSImageView, NSMutableParagraphStyle,
    NSParagraphStyleAttributeName, NSPopover, NSPopoverBehavior, NSStatusBarButton,
    NSTextAlignment, NSTextField, NSView, NSViewController,
};
use objc2_foundation::{
    NSAttributedString, NSData, NSMutableAttributedString, NSPoint, NSRange, NSRect, NSRectEdge,
    NSSize, NSString,
};

use super::{PopoverModel, PopoverRow, Sev};

// ---------------------------------------------------------------------------
// Layout constants (points — AppKit scales for HiDPI).
// ---------------------------------------------------------------------------

const WIDTH: f64 = 320.0;
const H_PAD: f64 = 12.0;
const ROW_H: f64 = 24.0;
const SEP_H: f64 = 11.0;
const V_PAD: f64 = 6.0;
const FIELD_H: f64 = 17.0;
const ICON_SZ: f64 = 16.0;
/// Width of the flush-right trailing column (`S% / W%` / countdown).
const TRAIL_W: f64 = 118.0;
/// Leading x for account email / action labels (past the checkmark column).
const TEXT_X: f64 = 30.0;
/// Leading x for the checkmark / group icon column.
const LEAD_X: f64 = 10.0;

// ---------------------------------------------------------------------------
// Main-thread dispatch cell: the row views' `mouseDown:` reaches back through
// this to run the click id and close the popover. Only ever touched on the
// main thread (AppKit guarantees mouse events arrive there), so a plain
// thread-local is sufficient and sound.
// ---------------------------------------------------------------------------

struct Dispatch {
    on_click: Box<dyn Fn(&str)>,
    popover: Retained<NSPopover>,
}

thread_local! {
    static DISPATCH: RefCell<Option<Dispatch>> = const { RefCell::new(None) };
}

fn dispatch_click(id: &str) {
    DISPATCH.with(|d| {
        if let Some(d) = d.borrow().as_ref() {
            (d.on_click)(id);
            // Close after acting — a click on a menu row dismisses the menu.
            unsafe { d.popover.performClose(None) };
        }
    });
}

// ---------------------------------------------------------------------------
// Custom views.
// ---------------------------------------------------------------------------

define_class!(
    // A top-down (flipped) container so we can lay rows out from y = 0 at the
    // top; AppKit's default coordinate origin is bottom-left.
    #[unsafe(super(NSView))]
    #[name = "UsagioPopoverContentView"]
    struct ContentView;

    impl ContentView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }
    }
);

struct RowIvars {
    click_id: Retained<NSString>,
}

define_class!(
    // A clickable, flipped row. Holds the click-id it should fire.
    #[unsafe(super(NSView))]
    #[name = "UsagioPopoverRow"]
    #[ivars = RowIvars]
    struct RowView;

    impl RowView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(mouseDown:))]
        fn on_mouse_down(&self, _event: &NSEvent) {
            let id = self.ivars().click_id.to_string();
            dispatch_click(&id);
        }
    }
);

// ---------------------------------------------------------------------------
// The popover host.
// ---------------------------------------------------------------------------

pub(crate) struct PopoverHost {
    popover: Retained<NSPopover>,
    controller: Retained<NSViewController>,
    button: Retained<NSStatusBarButton>,
    mtm: MainThreadMarker,
}

impl PopoverHost {
    /// Create the popover anchored to `button`. `on_click` is invoked with a
    /// row's click-id on `mouseDown:` (wire it to `menubar::handle_click`).
    /// `persistent` selects `ApplicationDefined` behavior (stays open — used by
    /// the `USAGIO_POPOVER_SHOW_ON_LAUNCH` screenshot path) instead of the
    /// normal `.transient` (auto-dismiss on click-outside / focus loss).
    // Currently uncalled: the 0.6.0 muri tray backend exposes no `NSStatusItem`
    // anchor, so `menubar::build_popover_host` returns `None` and never
    // constructs a host. Retained (behind the off-by-default `custom-popup`
    // feature) for when muri grows a status-item anchor — see that fn's note.
    #[allow(dead_code)]
    pub(crate) fn new(
        button: Retained<NSStatusBarButton>,
        on_click: Box<dyn Fn(&str)>,
        persistent: bool,
        mtm: MainThreadMarker,
    ) -> Self {
        let popover = NSPopover::init(mtm.alloc());
        popover.setBehavior(if persistent {
            NSPopoverBehavior::ApplicationDefined
        } else {
            NSPopoverBehavior::Transient
        });
        popover.setAnimates(true);

        let controller = NSViewController::init(mtm.alloc());
        // Seed with an empty view so the controller has something before the
        // first `show` rebuilds it.
        let empty = make_content_view(&PopoverModel::default(), mtm);
        controller.setView(&empty.0);
        popover.setContentViewController(Some(&controller));

        DISPATCH.with(|d| {
            *d.borrow_mut() = Some(Dispatch {
                on_click,
                popover: popover.clone(),
            });
        });

        PopoverHost {
            popover,
            controller,
            button,
            mtm,
        }
    }

    pub(crate) fn is_shown(&self) -> bool {
        self.popover.isShown()
    }

    /// Rebuild the content from `model` and show the popover below the status
    /// item button. No-op safe to call repeatedly; if already shown it updates
    /// in place.
    pub(crate) fn show(&self, model: &PopoverModel) {
        let (view, size) = make_content_view(model, self.mtm);
        self.controller.setView(&view);
        self.popover.setContentSize(size);
        let bounds = self.button.bounds();
        let anchor: &NSView = self.button.as_ref();
        self.popover
            .showRelativeToRect_ofView_preferredEdge(bounds, anchor, NSRectEdge::MinY);
    }

    pub(crate) fn close(&self) {
        unsafe { self.popover.performClose(None) };
    }

    /// Left-click handler: open with fresh content, or close if already open.
    pub(crate) fn toggle(&self, model: &PopoverModel) {
        if self.is_shown() {
            self.close();
        } else {
            self.show(model);
        }
    }
}

// ---------------------------------------------------------------------------
// View construction.
// ---------------------------------------------------------------------------

/// Build the flipped content view for `model` and return it plus its size (for
/// `setContentSize`). Lays rows out top-to-bottom in points.
fn make_content_view(model: &PopoverModel, mtm: MainThreadMarker) -> (Retained<NSView>, NSSize) {
    // Measure total height first.
    let mut height = V_PAD * 2.0;
    for row in &model.rows {
        height += match row {
            PopoverRow::Separator => SEP_H,
            _ => ROW_H,
        };
    }

    let container: Retained<ContentView> = unsafe {
        msg_send![mtm.alloc::<ContentView>(), initWithFrame: NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, height))]
    };
    let container_view: &NSView = &container;

    let mut y = V_PAD;
    for row in &model.rows {
        match row {
            PopoverRow::GroupHeader { title, icon_slug } => {
                let row_view = plain_row(
                    NSRect::new(NSPoint::new(0.0, y), NSSize::new(WIDTH, ROW_H)),
                    mtm,
                );
                // Provider icon (16px), vertically centered in the row.
                let mut text_x = H_PAD;
                if let Some(img) = icon_slug.and_then(|s| image_for_slug(s, mtm)) {
                    let iv = NSImageView::imageViewWithImage(&img, mtm);
                    iv.setFrame(NSRect::new(
                        NSPoint::new(LEAD_X, (ROW_H - ICON_SZ) / 2.0),
                        NSSize::new(ICON_SZ, ICON_SZ),
                    ));
                    row_view.addSubview(&iv);
                    text_x = TEXT_X;
                }
                let label = label_field(title, bold_font(), &NSColor::labelColor(), mtm);
                label.setFrame(field_rect(text_x, WIDTH - text_x - H_PAD));
                row_view.addSubview(&label);
                container_view.addSubview(&row_view);
            }
            PopoverRow::Account {
                email,
                active,
                trailing,
                colors,
                click_id,
            } => {
                let row_view = clickable_row(
                    NSRect::new(NSPoint::new(0.0, y), NSSize::new(WIDTH, ROW_H)),
                    click_id,
                    mtm,
                );
                // Leading checkmark for the active account, in accent color.
                if *active {
                    let check = label_field("✓", bold_font(), &NSColor::controlAccentColor(), mtm);
                    check.setFrame(field_rect(LEAD_X, TEXT_X - LEAD_X));
                    row_view.addSubview(&check);
                }
                // Email — bold when active. Stops short of the trailing column.
                let trail_x = WIDTH - H_PAD - TRAIL_W;
                let email_font = if *active { bold_font() } else { regular_font() };
                let email_label = label_field(email, email_font, &NSColor::labelColor(), mtm);
                email_label.setFrame(field_rect(TEXT_X, trail_x - TEXT_X - 6.0));
                row_view.addSubview(&email_label);
                // Trailing S% / W% (or countdown), flush-right with severity
                // colors — NO chevron column after it.
                let trail = trailing_field(trailing, colors, mtm);
                trail.setFrame(field_rect(trail_x, TRAIL_W));
                row_view.addSubview(&trail);
                container_view.addSubview(&row_view);
            }
            PopoverRow::Separator => {
                let sep: Retained<NSBox> = unsafe {
                    msg_send![mtm.alloc::<NSBox>(), initWithFrame: NSRect::new(
                        NSPoint::new(H_PAD, y + (SEP_H / 2.0)),
                        NSSize::new(WIDTH - H_PAD * 2.0, 1.0),
                    )]
                };
                sep.setBoxType(NSBoxType::Separator);
                container_view.addSubview(&sep);
            }
            PopoverRow::Action { label, click_id } => {
                let row_view = clickable_row(
                    NSRect::new(NSPoint::new(0.0, y), NSSize::new(WIDTH, ROW_H)),
                    click_id,
                    mtm,
                );
                let lbl = label_field(label, regular_font(), &NSColor::labelColor(), mtm);
                lbl.setFrame(field_rect(H_PAD, WIDTH - H_PAD * 2.0));
                row_view.addSubview(&lbl);
                container_view.addSubview(&row_view);
            }
        }
        y += match row {
            PopoverRow::Separator => SEP_H,
            _ => ROW_H,
        };
    }

    let view: Retained<NSView> = Retained::into_super(container);
    (view, NSSize::new(WIDTH, height))
}

/// A non-clickable flipped row container.
fn plain_row(frame: NSRect, mtm: MainThreadMarker) -> Retained<ContentView> {
    unsafe { msg_send![mtm.alloc::<ContentView>(), initWithFrame: frame] }
}

/// A clickable flipped row carrying `click_id`.
fn clickable_row(frame: NSRect, click_id: &str, mtm: MainThreadMarker) -> Retained<RowView> {
    let alloc = mtm.alloc::<RowView>().set_ivars(RowIvars {
        click_id: NSString::from_str(click_id),
    });
    unsafe { msg_send![super(alloc), initWithFrame: frame] }
}

/// Vertically-centered field rect of the standard row text height.
fn field_rect(x: f64, w: f64) -> NSRect {
    NSRect::new(
        NSPoint::new(x, (ROW_H - FIELD_H) / 2.0),
        NSSize::new(w.max(0.0), FIELD_H),
    )
}

fn regular_font() -> Retained<NSFont> {
    NSFont::systemFontOfSize(13.0)
}

fn bold_font() -> Retained<NSFont> {
    NSFont::boldSystemFontOfSize(13.0)
}

/// A non-editable single-line label with `text` in `color`/`font`.
fn label_field(
    text: &str,
    font: Retained<NSFont>,
    color: &NSColor,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFont(Some(&font));
    field.setTextColor(Some(color));
    field
}

/// The flush-right trailing field: `labelColor` base, severity-colored spans,
/// right-aligned via a paragraph style so alignment holds for the attributed
/// string.
fn trailing_field(
    text: &str,
    colors: &[super::PctSpan],
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let ns = NSString::from_str(text);
    let full_len = ns.length();
    let attr = NSMutableAttributedString::initWithString(NSMutableAttributedString::alloc(), &ns);
    let font = regular_font();
    let para = NSMutableParagraphStyle::new();
    para.setAlignment(NSTextAlignment::Right);
    unsafe {
        attr.addAttribute_value_range(NSFontAttributeName, &font, NSRange::new(0, full_len));
        attr.addAttribute_value_range(
            NSParagraphStyleAttributeName,
            &para,
            NSRange::new(0, full_len),
        );
        attr.addAttribute_value_range(
            NSForegroundColorAttributeName,
            &NSColor::labelColor(),
            NSRange::new(0, full_len),
        );
        for span in colors {
            if span.len == 0 || span.start >= full_len {
                continue;
            }
            let end = (span.start + span.len).min(full_len);
            let color = match span.sev {
                Sev::Amber => NSColor::systemOrangeColor(),
                Sev::Red => NSColor::systemRedColor(),
            };
            attr.addAttribute_value_range(
                NSForegroundColorAttributeName,
                &color,
                NSRange::new(span.start, end - span.start),
            );
        }
    }
    let attr: Retained<NSAttributedString> = Retained::into_super(attr);
    let field = NSTextField::labelWithAttributedString(&attr, mtm);
    field.setAlignment(NSTextAlignment::Right);
    field
}

/// Decode a bundled 16px provider PNG into a sized `NSImage`.
fn image_for_slug(slug: &str, _mtm: MainThreadMarker) -> Option<Retained<NSImage>> {
    let bytes = crate::icons::png16_for(slug)?;
    let data = NSData::with_bytes(bytes);
    let img = NSImage::initWithData(NSImage::alloc(), &data)?;
    img.setSize(NSSize::new(ICON_SZ, ICON_SZ));
    Some(img)
}
