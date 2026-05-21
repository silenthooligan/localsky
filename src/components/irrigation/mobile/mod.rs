// Mobile-tuned irrigation views. Activated by IrrigationPage when the
// is_mobile viewport signal is true; the existing bento renders on desktop.
//
// Sub-tabs share the /irrigation route and dispatch on the `tab` query param:
//   ?tab=now (or no query)  -> MobileNow      (overview, hero, advisor)
//   ?tab=zones              -> MobileZones    (list, tap-to-detail)
//   ?tab=schedule           -> MobileSchedule (verdict, history, settings)
//
// Zone detail is a separate route (/irrigation/zone/:slug) handled in app.rs.

pub mod duration_sheet;
pub mod layout;
pub mod now;
pub mod schedule;
pub mod stop_confirm;
pub mod zone_detail;
pub mod zones;

pub use layout::MobileIrrigation;
pub use zone_detail::MobileZoneDetail;
