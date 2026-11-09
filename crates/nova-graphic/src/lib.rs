//! `nova-graphic` is the NovaOS widget library. It is a trimmed-down Qt6
//! inspired object hierarchy for the desktop: every widget is a value (no
//! allocations, no globals), tracks its own state, and renders onto a
//! [`nova_ui::Canvas`].
//!
//! # Why a separate crate from `nova-ui`
//!
//! `nova-ui` is the low-level drawing substrate — rects, canvases, text —
//! whereas `nova-graphic` is the widget set that uses those primitives to
//! build what people expect a desktop toolkit to have: a button you can
//! click, a check-box that stays checked, a progress bar that fills, a list
//! you can scroll, a line edit that accepts typed characters.
//!
//! # The contract
//!
//! Every widget implements [`Widget`]. The widget is independent of the
//! window system; it receives events as plain numbers and coordinates, draws
//! itself onto any `Canvas` the caller supplies, and exposes its state as
//! public fields so persistence/restore is trivial.

#![no_std]

pub mod button;
pub mod checkbox;
pub mod colour;
pub mod container;
pub mod event;
pub mod label;
pub mod lineedit;
pub mod list;
pub mod progress;
pub mod radio;
pub mod slider;
pub mod widget;

pub use button::QPushButton;
pub use checkbox::QCheckBox;
pub use colour::Colour;
pub use container::VBox;
pub use event::{Event, EventKind, MouseButton, MouseState};
pub use label::QLabel;
pub use lineedit::{EchoMode, QLineEdit};
pub use list::QListWidget;
pub use progress::QProgressBar;
pub use radio::QRadioButton;
pub use slider::QSlider;
pub use widget::Rect;
pub use widget::Widget;
