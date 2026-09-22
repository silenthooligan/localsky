# Weekly scheduling

Weekly scheduling is an alternative to the soil model. It allocates a zone's weekly target after accounting for rain and recorded watering.

Check **Scheduling model** in Engine settings and the zone editor. A zone can follow the engine default or use its own model.

## How the balance works

The weekly calculation considers:

- the zone's weekly target;
- irrigation already applied;
- observed rain, capped per day by what the zone can bank;
- eligible forecast credit;
- the sessions still available.

A covered target, a forecast defer, a spacing hold, or an Override schedule can produce zero planned minutes. The zone reason tells you which applies.

## Set a target

Set **Weekly target** and **Sessions per week** deliberately for weekly-governed zones. Defaults are starting values, not a measurement of your property's needs.

The target is a configured depth. It does not become a continuously recalculated ET target just because ET is displayed elsewhere.

The seasonal adjustment scales the allocated run, and the zone's duration limit can cap it. Check the final minutes in the zone detail.

## Rain the soil can bank

The daily rain credit defaults to a capacity derived from soil and root depth. You can override it with **Rain the soil can bank per day**.

For illustration, the catalog's sand profile with 150 mm roots stores 9 mm, about 0.35 inches, between field capacity and wilting point. A 1.2-inch storm is not credited as 1.2 inches retained in that shallow root zone.

The daily cap changes with soil and roots. It is not a universal rainfall cutoff for every yard.

## Spacing and manual schedules

Completed watering also affects session spacing for weekly-governed zones. A frequent Floor schedule can cover the budget or leave no eligible smart session.

An Override schedule replaces smart scheduling for its zone on the covered days. To return a zone to fully automatic planning, disable the schedule and check the new plan.

[Manual schedules](schedules.md)

## How the soil model differs

The soil model schedules from reconstructed depletion and the zone's trigger. Its cadence follows demand and storage rather than sessions per week.

An explicitly configured weekly target remains a rolling delivery ceiling for soil scheduling. A zone without enough soil configuration or evidence can use a fallback rather than a claimed precise deficit.

[Soil model](irrigation-engine.md#the-soil-model) · [Zone duration](zone-math.md)
