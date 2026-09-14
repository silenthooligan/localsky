// The systems LocalSky talks to that are not weather sources or
// irrigation controllers.
//
// A source publishes readings and a controller opens valves; both have
// their own port and their own directory. What is left is the software
// someone already runs and wants LocalSky to fit into. Home Assistant is
// the first and, so far, the only one.

#[cfg(feature = "ssr")]
pub mod home_assistant;
