// Mobile-tuned irrigation views. Activated by IrrigationPage when the
// is_mobile viewport signal is true; the existing bento renders on desktop.
//
// The mobile /irrigation page renders the "Now" overview (hero, advisor,
// controls). Zones and history are top-level routes (/zones, /history).

pub mod layout;
pub mod now;
pub mod stop_confirm;

pub use layout::MobileIrrigation;
