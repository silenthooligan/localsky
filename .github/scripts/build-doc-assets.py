#!/usr/bin/env python3
"""Build the checked-in API read profile and AI documentation exports."""
import argparse
import json
from pathlib import Path
import re
import sys
import tomllib

ROOT = Path(__file__).resolve().parents[2]
DOCS = ROOT / 'docs/src'


def scalar(kind, nullable=False, **extra):
    return {'type': [kind, 'null'] if nullable else kind, **extra}


def ref(name):
    return {'$ref': '#/components/schemas/' + name}


def array(items):
    return {'type': 'array', 'items': items}


def obj(properties, required=None, **extra):
    result = {'type': 'object', 'properties': properties, 'additionalProperties': True, **extra}
    if required:
        result['required'] = required
    return result


def profile(version):
    integer = scalar('integer', format='int64')
    number = scalar('number')
    text = scalar('string')
    boolean = scalar('boolean')
    ni = scalar('integer', True, format='int64')
    nn = scalar('number', True)
    ns = scalar('string', True)
    schemas = {}
    info = {key: text for key in ['service', 'service_version', 'build_revision', 'api_version', 'api_prefix', 'license', 'repository']}
    info.update({key: boolean for key in ['dry_run', 'demo', 'auth_required', 'has_irrigation', 'nerd_mode_default', 'location_configured']})
    info['uuid'] = ns
    schemas['Info'] = obj(info, ['service', 'service_version', 'api_version', 'api_prefix', 'auth_required'])
    sample = {'value': number, 'source_id': text, 'observed_epoch': integer, 'max_age_s': integer, 'measured': boolean, 'selection_reason': text}
    schemas['CurrentWeatherSample'] = obj(sample, list(sample))
    weather = {key: number for key in ['air_temp_f', 'rh_pct', 'wind_avg_mph', 'wind_gust_mph', 'rain_in_today', 'rain_intensity_in_hr']}
    weather.update({key: integer for key in ['last_packet_epoch', 'air_temp_live_epoch', 'wind_live_epoch', 'rh_live_epoch', 'rain_live_epoch']})
    weather.update({'source_label': text, 'has_live_station': boolean})
    schemas['WeatherSnapshot'] = obj(weather, description='Selected legacy weather fields. Numeric values require their field-specific observation/validity context; zero alone is not evidence of a measurement. Additional fields are retained.')
    hour = {'time_epoch': integer, 'temp_f': nn, 'precip_in': nn, 'precip_probability': scalar('integer', True, minimum=0, maximum=100)}
    schemas['WindowHour'] = obj(hour, list(hour))
    window = {'track': text, 'model': ns, 'provider_label': text, 'fetched_at': ni, 'age_s': ni, 'from': integer, 'to': integer, 'complete': boolean, 'hourly': array(ref('WindowHour'))}
    window.update({key: scalar('integer', minimum=0) for key in ['hours', 'expected_hours', 'precipitation_hours', 'probability_hours', 'temperature_hours']})
    window.update({key: nn for key in ['precip_max_in', 'precip_sum_in', 'temp_max_f', 'temp_min_f']})
    window['pop_max_pct'] = scalar('integer', True, minimum=0, maximum=100)
    schemas['ForecastWindow'] = obj(window, list(window), description='Inclusive hour-start query. Complete checks timestamps; also check required nullable measurements and age_s. Rain uses inches; temperature uses Fahrenheit.')
    forecast = {'last_refresh_epoch': integer, 'source_reachable': boolean, 'source_label': text, 'source_is_backup': boolean, 'timezone': text, 'daily': array(obj({})), 'past_daily': array(obj({})), 'hourly': array(ref('WindowHour'))}
    schemas['ForecastSnapshot'] = obj(forecast, list(forecast), description='Selected forecast. Hourly objects contain additional fields beyond the read profile; daily entries are intentionally open objects. Use forecast/window for a compact typed interval.')
    zone = {'name': text, 'slug': text, 'running': boolean, 'running_known': boolean, 'running_observed_epoch': ni, 'ledger_running': boolean, 'controller_id': ns, 'planned_run_seconds': integer, 'bucket_mm': nn}
    schemas['Zone'] = obj(zone, ['name', 'slug', 'running', 'running_known'], description='Read running together with running_known. A planned runtime is not delivered water.')
    pzone = {key: text for key in ['zone', 'name', 'reason', 'reason_code', 'water_need', 'demand_source', 'model']}
    pzone.update({'planned_seconds': integer, 'depletion_mm': nn, 'trigger_mm': nn, 'capacity_mm': nn, 'demand_mm': number, 'session_capped': boolean, 'depletion_range_mm': {'type': ['array', 'null'], 'items': number, 'minItems': 2, 'maxItems': 2}})
    schemas['WaterPlanZone'] = obj(pzone, list(pzone))
    plan = {'date_local': text, 'day_offset': integer, 'time_epoch': integer, 'start_epoch': ni, 'finish_epoch': ni, 'forecast_rain_mm': nn, 'expected_rain_mm': nn, 'rain_probability_pct': scalar('integer', True), 'evidence_complete': boolean, 'zones': array(ref('WaterPlanZone'))}
    schemas['WaterPlanDay'] = obj(plan, list(plan), description='Future scenario, not a dispatched command or a record of applied water.')
    irrigation = {'last_refresh_epoch': integer, 'timezone': text, 'restart_required': boolean, 'restart_reasons': array(text), 'next_run_epoch': integer, 'next_run_state': {'type': 'string', 'enum': ['at', 'no_water_planned', 'no_legal_day', 'no_sunrise', 'no_location']}, 'zones': array(ref('Zone')), 'water_plan': array(ref('WaterPlanDay')), 'decision_trace': {'type': ['object', 'null'], 'additionalProperties': True}, 'current_weather': {'type': ['object', 'null'], 'additionalProperties': ref('CurrentWeatherSample')}}
    schemas['IrrigationSnapshot'] = obj(irrigation, ['last_refresh_epoch', 'zones'], description='Selected fields from the full irrigation snapshot. Read observation ages independently of snapshot age.')
    run = {'session_id': ns, 'zone': text, 'start_epoch': integer, 'duration_s': integer, 'skip_reason': ns, 'source': text, 'status': text, 'controller_id': ns, 'applied_mm': nn, 'volume_gal': nn, 'note': ns, 'cycle_index': ni, 'cycle_count': ni}
    schemas['RunRecord'] = obj(run, ['zone', 'start_epoch', 'duration_s', 'skip_reason'], description='Use session identity and union of valve-open intervals to avoid double-counting. Dry-run and skip records are not applied water.')
    daily_zone = {key: text for key in ['zone', 'name', 'reason_code', 'reason', 'water_need']}
    daily_zone['planned_seconds'] = integer
    schemas['DailyZone'] = obj(daily_zone, list(daily_zone))
    daily = {'date_local': text, 'epoch': integer, 'kind': text, 'zones': array(ref('DailyZone'))}
    schemas['DailyDecision'] = obj(daily, list(daily))
    schemas['HistoryWindow'] = obj({'from_epoch': integer, 'to_epoch': integer, 'runs': array(ref('RunRecord')), 'daily': array(ref('DailyDecision'))}, ['from_epoch', 'to_epoch', 'runs', 'daily'])
    archive = {'track': text, 'provider': text, 'model': ns, 'target_epoch': integer, 'lead_h': integer, 'pop_pct': scalar('integer', True), 'precip_in': nn, 'fetched_at': integer}
    schemas['ArchiveRow'] = obj(archive, list(archive))
    schemas['ArchivePage'] = obj({'rows': array(ref('ArchiveRow')), 'next_cursor': ns}, ['rows', 'next_cursor'])
    schemas['Error'] = obj({'error': text, 'code': text, 'diagnostic': obj({}), 'request': obj({'id': text, 'method': text, 'route': text})}, description='Error fields vary by endpoint. Preserve HTTP status and X-LocalSky-Request-Id. Diagnostic evidence is optional.')
    paths = {}

    def query(name, schema, description, required=False):
        return {'name': name, 'in': 'query', 'required': required, 'description': description, 'schema': schema}

    def get(path, operation, summary, schema, parameters=None, public=False, description=''):
        operation_data = {'operationId': operation, 'summary': summary, 'description': description or summary, 'responses': {'200': {'description': 'Successful read; inspect data freshness, nulls, and coverage.', 'content': {'application/json': {'schema': schema}}}, 'default': {'description': 'Request failure. Errors can also be generated by a proxy.', 'headers': {'X-LocalSky-Request-Id': {'schema': text, 'description': 'Request correlation ID when supplied by LocalSky.'}}, 'content': {'application/json': {'schema': ref('Error')}}}}}
        if parameters:
            operation_data['parameters'] = parameters
        if public:
            operation_data['security'] = []
        paths[path] = {'get': operation_data}

    get('/info', 'getLocalSkyInfo', 'Identify the server and API version', ref('Info'), public=True)
    get('/snapshot', 'getCurrentWeather', 'Read current weather with validity context', ref('WeatherSnapshot'))
    get('/forecast/snapshot', 'getSelectedForecast', 'Read the selected daily and hourly forecast', ref('ForecastSnapshot'))
    track = query('track', scalar('string', default='merged'), 'merged or a configured extra model ID.')
    bounds = [query('from', scalar('integer', format='int64', minimum=0), 'First UTC epoch, inclusive.', True), query('to', scalar('integer', format='int64', minimum=0), 'Last UTC epoch, inclusive.', True)]
    get('/forecast/window', 'getForecastWindow', 'Read a forecast interval', ref('ForecastWindow'), [track, *bounds], description='At most 48 hours between bounds. Each selected hourly timestamp starts an interval of one hour. Check age_s, complete, and required summary values. API 2.3.0+.')
    get('/forecast/tracks', 'listForecastTracks', 'List configured extra forecast models', array(obj({})), description='Returns track identity, age, serving state, and errors. Extra tracks do not drive irrigation. API 2.3.0+.')
    get('/forecast/archive', 'getForecastArchive', 'Read recorded forecast issuances', ref('ArchivePage'), [track, *bounds, query('lead_h', scalar('integer', minimum=0, maximum=47), 'Optional forecast lead hour.'), query('limit', scalar('integer', minimum=1, maximum=5000, default=1000), 'Maximum rows in this page.'), query('cursor', text, 'Returned next_cursor; retain the other query parameters.')], description='Inclusive target-hour range up to 400 days. Keep forecasts separate from measured rain. This profile uses JSON. API 2.3.0+.')
    get('/irrigation/snapshot', 'getIrrigationState', 'Read zones, current decisions, and projected water plans', ref('IrrigationSnapshot'))
    get('/irrigation/history', 'getWateringHistory', 'Read recorded runs and daily decisions', ref('HistoryWindow'), [query('days', scalar('integer', minimum=0, maximum=36500, default=30), 'Days of history; 0 requests all retained records.')])
    get('/health', 'getLocalSkyHealth', 'Read liveness and available health detail', obj({}), description='Anonymous callers receive reduced liveness. Send bearer credentials for full source diagnostics. A successful HTTP status alone does not establish healthy sources.')
    return {'openapi': '3.1.0', 'info': {'title': 'LocalSky API read profile', 'version': version, 'description': 'Selected read endpoints for dashboards, automation, and AI connectors. Additional response fields are allowed. This profile does not grant read-only credentials: LocalSky bearer tokens can authorize writes outside this profile.', 'license': {'name': 'Apache-2.0', 'identifier': 'Apache-2.0'}}, 'servers': [{'url': 'http://localhost:8090/api/v1', 'description': 'Replace with your reachable LocalSky instance, including /api/v1.'}], 'security': [{'LocalSkyToken': []}], 'paths': paths, 'components': {'securitySchemes': {'LocalSkyToken': {'type': 'http', 'scheme': 'bearer', 'description': 'API token from Settings > Account. Store as a secret. No native read-only scope.'}}, 'schemas': schemas}}


def outputs():
    version = re.search(r'pub const API_VERSION: &str = "([^"]+)"', (ROOT/'src/api/info.rs').read_text()).group(1)
    package = tomllib.loads((ROOT/'Cargo.toml').read_text())['package']['version']
    summary = (DOCS/'SUMMARY.md').read_text(encoding='utf-8')
    chapters = re.findall(r'\[([^\]]+)\]\(([^)]+\.md)\)', summary)
    index = ['# LocalSky', '', f'> Self-hosted weather and irrigation. Guide for service {package}, API {version}.', '', 'Use recorded history for past outcomes and water_plan for projections. Preserve nulls, units, source identity, observation age, and forecast coverage. LocalSky API tokens are not read-scoped.', '', '## Guides']
    full = [f'# LocalSky guide\n\nService {package}; API {version}.\n']
    for title, filename in chapters:
        index.append(f'- [{title}](https://localsky.io/docs/{filename[:-3]}.html): {title}.')
        page = (DOCS/filename).read_text(encoding='utf-8')
        page = page.replace('{{LOCALSKY_VERSION}}', package).replace('{{LOCALSKY_API_VERSION}}', version)
        page = re.sub(r'\{\{LOCALSKY_DB_MIGRATIONS\}\}', str(len(list((ROOT/'src/persistence/migrations').glob('M*.sql')))), page)
        page = page.replace('{{LOCALSKY_SKIP_RULES}}', str((ROOT/'src/gates_catalog.rs').read_text().count('        (\n')))
        full.append(f'\n---\n\nSource: https://localsky.io/docs/{filename[:-3]}.html\n\n{page}')
    index += ['', '## Connector files', '- [OpenAPI read profile](https://localsky.io/docs/openapi.json)', '- [Full guide text](https://localsky.io/docs/llms-full.txt)', '']
    return {'openapi.json': json.dumps(profile(version), indent=2, ensure_ascii=False)+'\n', 'llms.txt': '\n'.join(index), 'llms-full.txt': '\n'.join(full)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true', help='Fail if checked-in exports differ from sources.')
    args = parser.parse_args()
    stale = []
    for name, content in outputs().items():
        path = DOCS/name
        if args.check:
            if not path.exists() or path.read_text(encoding='utf-8') != content:
                stale.append(name)
        else:
            path.write_text(content, encoding='utf-8')
    if stale:
        print('Regenerate documentation exports: '+', '.join(stale), file=sys.stderr)
        return 1
    print('Documentation exports '+('verified' if args.check else 'generated'))
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
