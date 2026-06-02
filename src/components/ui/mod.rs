// Design-system primitives. Each wraps an SCSS class in main.scss; theme
// tokens drive every color so [data-theme="..."] switching applies
// automatically.
//
// Shipped:
//   icon.rs       - single app-wide inline-SVG registry (currentColor)
//   button.rs     - primary/secondary/ghost/danger button, sizes, loading
//   panel.rs      - container with optional title + badge
//   card.rs       - raised surface, optional clickable
//   sheet.rs      - viewport-aware bottom-sheet (mobile) / centered modal
//   toggle.rs     - iOS-style switch with label + helptext
//   slider.rs     - range + value chip with suffix
//   stepper.rs    - +/- integer-ish spinner
//   segmented.rs  - horizontal pill picker (radiogroup)
//   form_field.rs - label + helptext + error wrapper
//   list_item.rs  - icon + title + subtitle + trailing control/chevron
//   empty_state.rs - icon + title + body + CTA for empty pages
//   stat_tile.rs  - label + big number + delta + inline sparkline
//   sparkline.rs  - inline single-series SVG trend line
//   line_chart.rs - multi-series SVG chart (paths) + HTML legend/axes
//   toast.rs      - ToastHub context + ToastViewport stack
//   help_hint.rs  - tooltip/popover wrapper
//   photo_field.rs - file upload + preview

pub mod button;
pub mod card;
pub mod empty_state;
pub mod form_field;
pub mod help_hint;
pub mod icon;
pub mod line_chart;
pub mod list_item;
pub mod panel;
pub mod photo_field;
pub mod segmented;
pub mod sheet;
pub mod slider;
pub mod sparkline;
pub mod stat_tile;
pub mod stepper;
pub mod toast;
pub mod toggle;

pub use button::Button;
pub use card::Card;
pub use empty_state::EmptyState;
pub use form_field::FormField;
pub use help_hint::HelpHint;
pub use icon::{weather_glyph, Icon};
pub use line_chart::{LineChart, Series};
pub use list_item::ListItem;
pub use panel::Panel;
pub use photo_field::PhotoField;
pub use segmented::SegmentedControl;
pub use sheet::Sheet;
pub use slider::Slider;
pub use sparkline::Sparkline;
pub use stat_tile::{DeltaSense, StatTile};
pub use stepper::Stepper;
pub use toast::{use_toast, ToastHub, ToastKind, ToastViewport};
pub use toggle::Toggle;
