//! The Free Airport Mapping DB on Hugging Face, for when the AMDB Bridge is
//! not running: one airport file per ICAO code, downloaded on first use and
//! kept in the local AMDB folder in the plain lat/lon form [`super::AmdbProvider`]
//! reads. The data is OpenStreetMap's, under the ODbL.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

const DATASET: &str = "https://huggingface.co/datasets/Royale2072/free-airport-mapping-db/resolve/main";
const SPHERE_RADIUS_M: f64 = 6_371_008.8;
const RECEIVE_MS: u32 = 90_000;
const RETRY_AFTER: Duration = Duration::from_secs(600);
const INDEX_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
pub const ATTRIBUTION: &str = "Airport map data from the Free Airport Mapping DB (huggingface.co/datasets/Royale2072/free-airport-mapping-db): \
X-Plane Scenery Gateway data under the GPL 2.0, and \u{a9} OpenStreetMap contributors under the Open Database License (ODbL).";

#[derive(Clone, Debug, PartialEq)]
pub enum Fetch {
    Ready,
    Pending,
    Missing,
}

enum State {
    InFlight,
    Failed(Instant),
}

static DOWNLOADS: Mutex<Option<HashMap<PathBuf, State>>> = Mutex::new(None);

fn icao_code(icao: &str) -> Option<String> {
    let code: String = icao.trim().to_ascii_uppercase();
    (code.len() == 4 && code.chars().all(|c| c.is_ascii_alphanumeric())).then_some(code)
}

/// Format 2 of the dataset: the Scenery Gateway file, then the OpenStreetMap
/// one, which beside a Gateway file only carries the layers the Gateway lacks.
pub fn airport_urls(icao: &str) -> Option<[String; 2]> {
    let code = icao_code(icao)?;
    Some(["gateway", "osm"].map(|source| format!("{DATASET}/{source}/v1/{}/{code}.json", &code[..2])))
}

fn index_path(root: &Path, query: &str) -> (PathBuf, String) {
    match query.chars().next().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_uppercase()) {
        None => (root.join("index").join("search.json"), format!("{DATASET}/v1/search.json")),
        Some(c) => (root.join("index").join(format!("search-{c}.json")), format!("{DATASET}/v1/search/{c}.json")),
    }
}

fn fresh(path: &Path, max_age: Option<Duration>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else { return false };
    match max_age {
        None => true,
        Some(age) => meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|e| e < age),
    }
}

/// Fetches every url; a 404 is an absent file (`None`), any other failure fails the lot.
fn download(target: PathBuf, urls: Vec<String>, convert: fn(&[Option<String>]) -> Option<String>, max_age: Option<Duration>) -> Fetch {
    if fresh(&target, max_age) {
        return Fetch::Ready;
    }
    let mut guard = DOWNLOADS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = guard.get_or_insert_with(HashMap::new);
    match map.get(&target) {
        Some(State::InFlight) => return Fetch::Pending,
        Some(State::Failed(at)) if at.elapsed() < RETRY_AFTER => {
            return if target.exists() { Fetch::Ready } else { Fetch::Missing };
        }
        _ => {}
    }
    map.insert(target.clone(), State::InFlight);
    let key = target.clone();
    let spawned = std::thread::Builder::new().name("fbw-amdb-hf".into()).spawn(move || {
        let bodies: Option<Vec<Option<String>>> = urls
            .iter()
            .map(|url| match crate::net::request_with("GET", url, "", RECEIVE_MS)? {
                (200, body) => Some(Some(body)),
                (404, _) => Some(None),
                _ => None,
            })
            .collect();
        let ok = bodies.and_then(|b| convert(&b)).is_some_and(|text| write(&key, &text));
        let mut guard = DOWNLOADS.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(map) = guard.as_mut() {
            if ok {
                map.remove(&key);
            } else {
                map.insert(key, State::Failed(Instant::now()));
            }
        }
    });
    if spawned.is_err() {
        map.insert(target, State::Failed(Instant::now()));
        return Fetch::Missing;
    }
    Fetch::Pending
}

fn write(path: &Path, text: &str) -> bool {
    let Some(dir) = path.parent() else { return false };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let root = if dir.file_name().is_some_and(|n| n == "index") { dir.parent().unwrap_or(dir) } else { dir };
    let _ = std::fs::write(root.join("ATTRIBUTION.txt"), format!("{ATTRIBUTION}\n"));
    let partial = path.with_extension("part");
    std::fs::write(&partial, text).is_ok() && std::fs::rename(&partial, path).is_ok()
}

pub fn airport(root: &Path, icao: &str) -> Fetch {
    let (Some(code), Some(urls)) = (icao_code(icao), airport_urls(icao)) else { return Fetch::Missing };
    download(root.join(format!("{code}.json")), urls.into(), |bodies| merged(bodies).map(|v| v.to_string()), None)
}

/// Each file is projected about its own ARP, so they are merged once in lat/lon;
/// a later file only fills layers the earlier ones lack or leave empty.
fn merged(bodies: &[Option<String>]) -> Option<Value> {
    let mut out: Option<Map<String, Value>> = None;
    for body in bodies.iter().flatten() {
        let Some(Value::Object(layers)) = serde_json::from_str(body).ok().as_ref().and_then(to_local) else { continue };
        let Some(out) = out.as_mut() else {
            out = Some(layers);
            continue;
        };
        for (name, collection) in layers {
            let empty = out.get(&name).and_then(|c| c.get("features")).and_then(Value::as_array).is_none_or(Vec::is_empty);
            if empty {
                out.insert(name, collection);
            }
        }
    }
    out.map(Value::Object)
}

pub fn search_index(root: &Path, query: &str) -> Option<Vec<Value>> {
    let (path, url) = index_path(root, query);
    let index = |bodies: &[Option<String>]| {
        let body = bodies.first()?.as_ref()?;
        serde_json::from_str::<Vec<Value>>(body).ok().map(|_| body.clone())
    };
    match download(path.clone(), vec![url], index, Some(INDEX_MAX_AGE)) {
        Fetch::Pending => None,
        Fetch::Ready | Fetch::Missing => Some(
            std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok()).unwrap_or_default(),
        ),
    }
}

pub fn matches(entry: &Value, query: &str) -> bool {
    let q = query.trim().to_ascii_uppercase();
    q.is_empty()
        || ["idarpt", "iata", "name"]
            .iter()
            .filter_map(|k| entry.get(*k).and_then(Value::as_str))
            .any(|v| v.to_ascii_uppercase().contains(&q))
}

pub fn to_local(hf: &Value) -> Option<Value> {
    let arp = hf.get("arp")?.as_array()?;
    let origin = (arp.first()?.as_f64()?, arp.get(1)?.as_f64()?);
    let mut out = Map::new();
    for (name, collection) in hf.get("layers")?.as_object()? {
        let name = if name == "touchdownliftoffarea" { "touchdownliftofarea" } else { name.as_str() };
        let mut collection = collection.clone();
        if let Some(features) = collection.get_mut("features").and_then(Value::as_array_mut) {
            for feature in features {
                for pointer in ["/geometry/coordinates", "/properties/midpoint/coordinates", "/properties/centroid/coordinates"] {
                    if let Some(coordinates) = feature.pointer_mut(pointer) {
                        unproject_coordinates(coordinates, origin);
                    }
                }
            }
        }
        out.insert(name.to_string(), collection);
    }
    if let Some(feature) = out.get_mut("aerodromereferencepoint").and_then(|c| c.pointer_mut("/features/0")) {
        let elev = feature.pointer("/properties/elev").and_then(Value::as_f64);
        if let (Some(elev), Some(coordinates)) = (elev, feature.pointer_mut("/geometry/coordinates").and_then(Value::as_array_mut)) {
            coordinates.truncate(2);
            coordinates.push(json!(elev));
        }
    }
    Some(Value::Object(out))
}

fn from_arp_azeq(origin: (f64, f64), east: f64, north: f64) -> (f64, f64) {
    let (lat0, lon0) = (origin.0.to_radians(), origin.1.to_radians());
    let rho = east.hypot(north);
    if rho == 0.0 {
        return origin;
    }
    let c = rho / SPHERE_RADIUS_M;
    let lat = (c.cos() * lat0.sin() + north * c.sin() * lat0.cos() / rho).asin();
    let lon = lon0 + (east * c.sin()).atan2(rho * lat0.cos() * c.cos() - north * lat0.sin() * c.sin());
    (lat.to_degrees(), lon.to_degrees())
}

fn unproject_coordinates(value: &mut Value, origin: (f64, f64)) {
    let Value::Array(items) = value else { return };
    if (2..=3).contains(&items.len()) && items.iter().all(Value::is_number) {
        let (east, north) = (items[0].as_f64().unwrap_or(0.0), items[1].as_f64().unwrap_or(0.0));
        let (lat, lon) = from_arp_azeq(origin, east, north);
        let round = |v: f64| (v * 1e8).round() / 1e8;
        items[0] = json!(round(lon));
        items[1] = json!(round(lat));
        return;
    }
    for item in items.iter_mut() {
        unproject_coordinates(item, origin);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HF_AIRPORT: &str = r#"{
        "icao": "EXMP",
        "arp": [10.0, 20.0],
        "layers": {
            "aerodromereferencepoint": { "type": "FeatureCollection", "features": [
                { "type": "Feature", "properties": { "feattype": 26, "id": 1, "idarpt": "EXMP", "name": "EXAMPLE", "iata": "XMP", "elev": 100.0 },
                  "geometry": { "type": "Point", "coordinates": [0.0, 0.0] } } ] },
            "runwayelement": { "type": "FeatureCollection", "features": [
                { "type": "Feature", "properties": { "feattype": 0, "id": 2, "idrwy": "09.27", "centroid": { "type": "Point", "coordinates": [0.0, 0.0] } },
                  "geometry": { "type": "LineString", "coordinates": [[-547.6, 0.0], [547.6, 0.0]] } } ] },
            "touchdownliftoffarea": { "type": "FeatureCollection", "features": [] }
        }
    }"#;

    #[test]
    fn a_dataset_airport_becomes_a_local_lat_lon_file_that_projects_back_to_the_same_metres() {
        let local = to_local(&serde_json::from_str(HF_AIRPORT).unwrap()).unwrap();
        assert!(local.get("touchdownliftofarea").is_some(), "FlyByWire's spelling");
        assert!(local.get("touchdownliftoffarea").is_none());
        let arp = &local["aerodromereferencepoint"]["features"][0]["geometry"]["coordinates"];
        assert_eq!((arp[0].as_f64().unwrap(), arp[1].as_f64().unwrap(), arp[2].as_f64().unwrap()), (20.0, 10.0, 100.0));
        let end = &local["runwayelement"]["features"][0]["geometry"]["coordinates"][1];
        let (lon, lat) = (end[0].as_f64().unwrap(), end[1].as_f64().unwrap());
        assert!((lat - 10.0).abs() < 1e-6 && lon > 20.0, "547.6 m east of the ARP: {lat}, {lon}");
        let (east, north) = super::super::to_arp_azeq(10.0, 20.0, lat, lon);
        assert!((east - 547.6).abs() < 0.05 && north.abs() < 0.05, "{east} {north}");
        let centroid = &local["runwayelement"]["features"][0]["properties"]["centroid"]["coordinates"];
        assert_eq!((centroid[0].as_f64().unwrap(), centroid[1].as_f64().unwrap()), (20.0, 10.0));
    }

    #[test]
    fn the_inverse_projection_round_trips_far_from_the_reference_point() {
        for (east, north) in [(3000.0, -4000.0), (-12000.0, 7000.0), (0.0, 25000.0)] {
            let (lat, lon) = from_arp_azeq((51.4775, -0.4614), east, north);
            let (e, n) = super::super::to_arp_azeq(51.4775, -0.4614, lat, lon);
            assert!((e - east).abs() < 0.1 && (n - north).abs() < 0.1, "{east},{north} -> {e},{n}");
        }
    }

    #[test]
    fn files_are_named_as_the_dataset_lays_them_out() {
        assert_eq!(airport_urls("egll").unwrap(), [format!("{DATASET}/gateway/v1/EG/EGLL.json"), format!("{DATASET}/osm/v1/EG/EGLL.json")]);
        assert!(airport_urls("EGL").is_none());
        assert!(airport_urls("EG/L").is_none());
        let root = Path::new("amdb");
        assert_eq!(index_path(root, ""), (root.join("index").join("search.json"), format!("{DATASET}/v1/search.json")));
        assert_eq!(index_path(root, "lhr"), (root.join("index").join("search-L.json"), format!("{DATASET}/v1/search/L.json")));
    }

    #[test]
    fn the_osm_file_fills_only_the_layers_the_gateway_file_lacks() {
        let file = |arp: [f64; 2], layers: Value| Some(json!({ "arp": arp, "layers": layers }).to_string());
        let line = |e: f64| json!({ "type": "FeatureCollection", "features": [{ "geometry": { "type": "Point", "coordinates": [e, 0.0] } }] });
        let none = json!({ "type": "FeatureCollection", "features": [] });
        let gateway = file([10.0, 20.0], json!({ "runwayelement": line(100.0), "water": none }));
        let osm = file([10.001, 20.0], json!({ "runwayelement": line(999.0), "water": line(0.0), "serviceroad": line(0.0) }));
        let out = merged(&[gateway.clone(), osm.clone()]).unwrap();
        let lon = |layer: &str| out[layer]["features"][0]["geometry"]["coordinates"][0].as_f64().unwrap();
        assert!((lon("runwayelement") - 20.0).abs() < 0.01, "the Gateway runway stays");
        assert!((out["water"]["features"][0]["geometry"]["coordinates"][1].as_f64().unwrap() - 10.001).abs() < 1e-9, "OSM's own ARP");
        assert!(out.get("serviceroad").is_some());
        assert_eq!(merged(&[None, osm]).unwrap()["runwayelement"]["features"].as_array().unwrap().len(), 1, "OSM alone");
        assert!(merged(&[None, None]).is_none());
    }

    #[test]
    fn a_search_entry_matches_its_code_iata_or_name() {
        let entry = json!({ "idarpt": "EGLL", "iata": "LHR", "name": "London Heathrow" });
        for q in ["", "egll", "LHR", "heath"] {
            assert!(matches(&entry, q), "{q}");
        }
        assert!(!matches(&entry, "KJFK"));
        assert!(matches(&json!({ "idarpt": "XXXX", "iata": null, "name": "X" }), "xx"));
    }
}
