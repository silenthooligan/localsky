# Visual fixture and reviewed baselines

`visual-demo.json` contains only synthetic demo data. The visual smoke cases fix
the clock, America/New_York timezone, 1280×720 viewport, and DejaVu font families.
Every API read is intercepted; unknown reads and all writes fail the case.
The separate render cases still exercise the actual candidate API and hydration.

The five Linux baselines were reviewed and regenerated on msi from compiled
candidate `03bce7e1385d88348f80441d053d315aa59b2026`. This replaces baselines whose
date, demo phase, history, and fonts varied between runs. In particular, the
irrigation image now shows the same RAIN decision in Today and the hero, and the
hero's full decision and explanation fit inside its narrow column. All five
images were inspected before copying them into the repository. The existing
3% pixel threshold remains unchanged; screenshot updates are disabled in the
acceptance and pre-deployment gates.

Visual approval does not waive accessibility or interaction checks. The same
review found 4.25:1 contrast in scheduled zone pills; a separate stylesheet fix
lifts the text above its tinted background, and axe remains a required gate.
