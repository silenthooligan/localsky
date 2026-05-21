// NotificationSink dispatcher + sink implementations. Phase 7 onward
// migrates the current src/push/dispatcher.rs WebPush path into
// sinks/web_push.rs, then adds sinks/mqtt.rs, sinks/ntfy.rs, sinks/slack.rs,
// sinks/email.rs, sinks/log.rs.
//
// The edge_detector lives in src/persistence/ (DB-backed); this module
// owns only the fan-out from a NotificationEvent into N enabled sinks.
