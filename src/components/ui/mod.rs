// Design-system primitives. Each wraps an SCSS class already in
// main.scss; theme tokens drive every color so [data-theme="..."]
// switching applies automatically.
//
// Shipped:
//   panel.rs    - glass-morphism container with optional title + badge
//   card.rs     - claymorphic surface, optional clickable
//   sheet.rs    - viewport-aware bottom-sheet (mobile) / centered modal (desktop)
//   toggle.rs   - iOS-style switch with label + helptext
//   slider.rs   - range + value chip with suffix
//   segmented.rs - horizontal pill picker (radiogroup)
//   form_field.rs - label + helptext + error wrapper for any form input
//   empty_state.rs - icon + title + body + CTA for post-wizard empty pages
//
// Planned (future iteration):
//   modal.rs (always-centered variant of sheet)
//   stepper.rs (+/- integer)
//   list_item.rs (settings rows)
//   stat_tile.rs (big number + delta arrow)
//   sparkline.rs (inline mini chart)
//   test_button.rs (idle/loading/success/fail async button)

pub mod card;
pub mod empty_state;
pub mod form_field;
pub mod panel;
pub mod segmented;
pub mod sheet;
pub mod slider;
pub mod toggle;

pub use card::Card;
pub use empty_state::EmptyState;
pub use form_field::FormField;
pub use panel::Panel;
pub use segmented::SegmentedControl;
pub use sheet::Sheet;
pub use slider::Slider;
pub use toggle::Toggle;
