# Location

Latitude, longitude, and elevation anchor everything: sunrise and
sunset for scheduling, solar geometry for evapotranspiration, the
timezone (inferred offline from coordinates), forecast grid points,
and radar centering.

Set it once in the wizard, by address search or by coordinates.
Elevation is auto-resolved when omitted. Changing location later
(Settings > Hardware > Location) re-infers the timezone and re-anchors
the forecast sources on their next poll.

LocalSky is hemisphere-aware end to end: the FAO-56 solar math is
signed-latitude correct, species curves flip seasons south of the
equator, and polar-edge cases (no sunrise) fall back to fixed
scheduling gracefully.
