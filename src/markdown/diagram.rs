//! Diagram code blocks rendered to SVG, like github.com does: `mermaid`
//! (pure Rust renderer), `geojson` / `topojson` maps and ASCII `stl` models.

use std::f64::consts::PI;
use std::fmt::Write;
use std::sync::OnceLock;

use serde_json::Value;

/// Is `lang` a fenced-code language that is drawn instead of printed?
pub fn is_diagram(lang: &str) -> bool {
    matches!(lang, "mermaid" | "geojson" | "topojson" | "stl")
}

/// Render a diagram block to an SVG document, or None if the source is not
/// understood (the block is then shown as code).
pub fn render(lang: &str, src: &str, dark: bool) -> Option<String> {
    match lang {
        "mermaid" => mermaid(src, dark),
        "geojson" | "topojson" => {
            let v: Value = serde_json::from_str(src).ok()?;
            let mut geoms = Vec::new();
            if v["type"] == "Topology" {
                Topology::new(&v)?.objects(&v, &mut geoms);
            } else {
                geojson(&v, &mut geoms);
            }
            map(&geoms, dark)
        }
        "stl" => stl(src, dark),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mermaid

#[cfg(not(feature = "mermaid"))]
fn mermaid(_src: &str, _dark: bool) -> Option<String> {
    None // left to `mmdc`
}

#[cfg(feature = "mermaid")]
fn mermaid(src: &str, dark: bool) -> Option<String> {
    use mermaid_rs_renderer::{render_with_options, RenderOptions, Theme};
    let mut theme = if dark {
        Theme::dark()
    } else {
        Theme::mermaid_default()
    };
    theme.background = "none".into();
    if dark {
        // mermaid's dark pie palette is nearly black: unreadable on a dark page
        theme.pie_colors = [
            "#4493f8", "#3fb950", "#d29922", "#ab7df8", "#f85149", "#39c5cf", "#db61a2", "#e3b341",
            "#56d364", "#79c0ff", "#ff7b72", "#d2a8ff",
        ]
        .map(String::from);
    }
    let options = RenderOptions {
        theme,
        ..Default::default()
    };
    // The renderer is fed untrusted input: a panic must not take shw down.
    crate::app::quiet_panics(|| render_with_options(src, options).ok())
        .flatten()
        .filter(|svg| svg.contains("<svg"))
}

// ---------------------------------------------------------------------------
// GeoJSON / TopoJSON

type Pt = [f64; 2];

enum Geom {
    Point(Pt),
    Line(Vec<Pt>),
    /// Outer ring followed by holes.
    Polygon(Vec<Vec<Pt>>),
}

fn pt(v: &Value) -> Option<Pt> {
    let a = v.as_array()?;
    Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?])
}

fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

fn line(v: &Value) -> Vec<Pt> {
    items(v).filter_map(pt).collect()
}

fn geojson(v: &Value, out: &mut Vec<Geom>) {
    let c = &v["coordinates"];
    match v["type"].as_str().unwrap_or("") {
        "FeatureCollection" => items(&v["features"]).for_each(|f| geojson(f, out)),
        "Feature" => geojson(&v["geometry"], out),
        "GeometryCollection" => items(&v["geometries"]).for_each(|g| geojson(g, out)),
        "Point" => out.extend(pt(c).map(Geom::Point)),
        "MultiPoint" => out.extend(line(c).into_iter().map(Geom::Point)),
        "LineString" => out.push(Geom::Line(line(c))),
        "MultiLineString" => out.extend(items(c).map(|l| Geom::Line(line(l)))),
        "Polygon" => out.push(Geom::Polygon(items(c).map(line).collect())),
        "MultiPolygon" => out.extend(items(c).map(|p| Geom::Polygon(items(p).map(line).collect()))),
        _ => {}
    }
}

/// A TopoJSON topology: shared, delta-encoded arcs referenced by index.
struct Topology {
    arcs: Vec<Vec<Pt>>,
    /// Quantisation transform (scale, translate).
    transform: Option<(Pt, Pt)>,
}

impl Topology {
    fn new(v: &Value) -> Option<Topology> {
        let transform = match v.get("transform") {
            Some(t) => Some((pt(&t["scale"])?, pt(&t["translate"])?)),
            None => None,
        };
        let arcs = items(&v["arcs"])
            .map(|arc| {
                let (mut x, mut y) = (0.0, 0.0);
                line(arc)
                    .into_iter()
                    .map(|p| match transform {
                        Some((s, t)) => {
                            x += p[0];
                            y += p[1];
                            [x * s[0] + t[0], y * s[1] + t[1]]
                        }
                        None => p,
                    })
                    .collect()
            })
            .collect();
        Some(Topology { arcs, transform })
    }

    fn point(&self, v: &Value) -> Option<Pt> {
        let p = pt(v)?;
        Some(match self.transform {
            Some((s, t)) => [p[0] * s[0] + t[0], p[1] * s[1] + t[1]],
            None => p,
        })
    }

    /// Stitch a list of arc indices (negative = reversed) into one line.
    fn stitch(&self, v: &Value) -> Vec<Pt> {
        let mut out: Vec<Pt> = Vec::new();
        for i in items(v).filter_map(Value::as_i64) {
            let (idx, reversed) = if i < 0 { (!i, true) } else { (i, false) };
            let Some(arc) = self.arcs.get(idx as usize) else {
                continue;
            };
            // consecutive arcs share their end point
            let skip = usize::from(!out.is_empty());
            if reversed {
                out.extend(arc.iter().rev().skip(skip));
            } else {
                out.extend(arc.iter().skip(skip));
            }
        }
        out
    }

    fn polygon(&self, v: &Value) -> Geom {
        Geom::Polygon(items(v).map(|r| self.stitch(r)).collect())
    }

    fn geometry(&self, g: &Value, out: &mut Vec<Geom>) {
        let (a, c) = (&g["arcs"], &g["coordinates"]);
        match g["type"].as_str().unwrap_or("") {
            "GeometryCollection" => items(&g["geometries"]).for_each(|g| self.geometry(g, out)),
            "Point" => out.extend(self.point(c).map(Geom::Point)),
            "MultiPoint" => out.extend(items(c).filter_map(|p| self.point(p)).map(Geom::Point)),
            "LineString" => out.push(Geom::Line(self.stitch(a))),
            "MultiLineString" => out.extend(items(a).map(|l| Geom::Line(self.stitch(l)))),
            "Polygon" => out.push(self.polygon(a)),
            "MultiPolygon" => out.extend(items(a).map(|p| self.polygon(p))),
            _ => {}
        }
    }

    fn objects(&self, v: &Value, out: &mut Vec<Geom>) {
        if let Some(objects) = v["objects"].as_object() {
            objects.values().for_each(|g| self.geometry(g, out));
        }
    }
}

/// Country outlines (Natural Earth 1:110m, public domain) as a base map.
fn countries() -> &'static [Geom] {
    static LAND: OnceLock<Vec<Geom>> = OnceLock::new();
    LAND.get_or_init(|| {
        let gz: &[u8] = include_bytes!("../../assets/countries-110m.json.gz");
        let mut out = Vec::new();
        let parsed = serde_json::from_reader::<_, Value>(flate2::read::GzDecoder::new(gz));
        if let Ok(v) = parsed {
            if let Some(t) = Topology::new(&v) {
                t.objects(&v, &mut out);
            }
        }
        out
    })
}

const MAX_LAT: f64 = 85.0;

/// Web Mercator, in degrees, y growing southwards.
fn mercator(p: Pt) -> Pt {
    let lat = p[1].clamp(-MAX_LAT, MAX_LAT).to_radians();
    [p[0], -(PI / 4.0 + lat / 2.0).tan().ln().to_degrees()]
}

fn mercator_lat(y: f64) -> f64 {
    (2.0 * (-y.to_radians()).exp().atan() - PI / 2.0).to_degrees()
}

struct MapColors {
    sea: &'static str,
    land: &'static str,
    border: &'static str,
    grid: &'static str,
    text: &'static str,
    area: &'static str,
    line: &'static str,
}

fn map(geoms: &[Geom], dark: bool) -> Option<String> {
    const W: f64 = 720.0;
    const H: f64 = 400.0;
    let c = if dark {
        MapColors {
            sea: "#0d1117",
            land: "#262c36",
            border: "#3d444d",
            grid: "#9198a1",
            text: "#9198a1",
            area: "#ab7df8",
            line: "#4493f8",
        }
    } else {
        MapColors {
            sea: "#dfeaf6",
            land: "#fbfaf6",
            border: "#c8cdd3",
            grid: "#59636e",
            text: "#59636e",
            area: "#8250df",
            line: "#0969da",
        }
    };

    // bounding box of the features in projected coordinates
    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    let mut grow = |p: &Pt| {
        if p[0].is_finite() && p[1].is_finite() {
            let m = mercator(*p);
            for i in 0..2 {
                lo[i] = lo[i].min(m[i]);
                hi[i] = hi[i].max(m[i]);
            }
        }
    };
    for g in geoms {
        match g {
            Geom::Point(p) => grow(p),
            Geom::Line(l) => l.iter().for_each(&mut grow),
            Geom::Polygon(rings) => rings.iter().flatten().for_each(&mut grow),
        }
    }
    if lo[0] > hi[0] {
        return None;
    }
    // view: the features with a generous margin, at the canvas aspect ratio
    let span_x = ((hi[0] - lo[0]) * 1.8)
        .max((hi[1] - lo[1]) * 1.8 * W / H)
        .clamp(0.05, 360.0);
    let scale = W / span_x;
    let (cx, cy) = ((lo[0] + hi[0]) / 2.0, (lo[1] + hi[1]) / 2.0);
    let (x0, y0) = (cx - span_x / 2.0, cy - H / scale / 2.0);
    let project = |p: &Pt| {
        let m = mercator(*p);
        [(m[0] - x0) * scale, (m[1] - y0) * scale]
    };
    // SVG path data for rings; None if the shape lies entirely outside the view
    let path = |rings: &[Vec<Pt>], close: bool| -> Option<String> {
        let mut d = String::new();
        let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
        for ring in rings {
            for (i, p) in ring.iter().enumerate() {
                let q = project(p);
                for k in 0..2 {
                    lo[k] = lo[k].min(q[k]);
                    hi[k] = hi[k].max(q[k]);
                }
                let _ = write!(
                    d,
                    "{}{:.1} {:.1}",
                    if i == 0 { 'M' } else { 'L' },
                    q[0],
                    q[1]
                );
            }
            if close && !ring.is_empty() {
                d.push('Z');
            }
        }
        (lo[0] <= W && hi[0] >= 0.0 && lo[1] <= H && hi[1] >= 0.0).then_some(d)
    };

    let mut s = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}"><defs><clipPath id="c"><rect width="{W}" height="{H}" rx="8"/></clipPath></defs><g clip-path="url(#c)" stroke-linejoin="round" stroke-linecap="round"><rect width="{W}" height="{H}" fill="{}"/>"#,
        c.sea
    );
    let _ = write!(
        s,
        r#"<g fill="{}" stroke="{}" stroke-width="1">"#,
        c.land, c.border
    );
    for g in countries() {
        if let Geom::Polygon(rings) = g {
            if let Some(d) = path(rings, true) {
                let _ = write!(s, r#"<path d="{d}"/>"#);
            }
        }
    }
    s.push_str("</g>");

    // graticule with coordinate labels
    let step = [
        0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 45.0,
    ]
    .into_iter()
    .find(|st| span_x / st <= 8.0)
    .unwrap_or(90.0);
    let label = |v: f64, pos: char, neg: char| {
        let digits = if step < 0.1 {
            2
        } else {
            usize::from(step < 1.0)
        };
        let hemi = if v > 0.0 { pos } else { neg };
        if v.abs() < step / 2.0 {
            "0°".to_string()
        } else {
            format!("{:.digits$}°{hemi}", v.abs())
        }
    };
    let _ = write!(
        s,
        r#"<g stroke="{}" stroke-opacity="0.25" stroke-width="1" fill="{}" font-family="Helvetica Neue, Helvetica, Noto Sans, DejaVu Sans, Arial, sans-serif" font-size="11">"#,
        c.grid, c.text
    );
    let mut lon = (x0 / step).ceil() * step;
    while lon < x0 + span_x {
        let x = (lon - x0) * scale;
        let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
        let _ = write!(
            s,
            r#"<path d="M{x:.1} 0V{H}"/><text x="{:.1}" y="{:.1}" stroke="none">{}</text>"#,
            x + 4.0,
            H - 6.0,
            label(wrapped, 'E', 'W')
        );
        lon += step;
    }
    let (north, south) = (mercator_lat(y0), mercator_lat(y0 + H / scale));
    let mut lat = (south / step).ceil() * step;
    while lat < north && lat <= MAX_LAT {
        let y = (mercator([0.0, lat])[1] - y0) * scale;
        let _ = write!(
            s,
            r#"<path d="M0 {y:.1}H{W}"/><text x="6" y="{:.1}" stroke="none">{}</text>"#,
            y - 4.0,
            label(lat, 'N', 'S')
        );
        lat += step;
    }
    s.push_str("</g>");

    // the features: areas first, then lines, points on top
    for g in geoms {
        if let Geom::Polygon(rings) = g {
            if let Some(d) = path(rings, true) {
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="{0}" fill-opacity="0.3" fill-rule="evenodd" stroke="{0}" stroke-width="2"/>"#,
                    c.area
                );
            }
        }
    }
    for g in geoms {
        if let Geom::Line(l) = g {
            if let Some(d) = path(std::slice::from_ref(l), false) {
                let _ = write!(
                    s,
                    r#"<path d="{d}" fill="none" stroke="{}" stroke-width="2.5"/>"#,
                    c.line
                );
            }
        }
    }
    for g in geoms {
        if let Geom::Point(p) = g {
            let q = project(p);
            let _ = write!(
                s,
                r#"<circle cx="{:.1}" cy="{:.1}" r="5.5" fill="{}" stroke="white" stroke-width="2"/>"#,
                q[0], q[1], c.line
            );
        }
    }
    s.push_str("</g></svg>");
    Some(s)
}

// ---------------------------------------------------------------------------
// STL

type V3 = [f64; 3];

fn sub(a: V3, b: V3) -> V3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: V3, b: V3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: V3, b: V3) -> V3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn unit(a: V3) -> V3 {
    let n = dot(a, a).sqrt();
    if n > 0.0 {
        [a[0] / n, a[1] / n, a[2] / n]
    } else {
        a
    }
}

/// Flat-shaded orthographic view of an ASCII STL solid (z up).
fn stl(src: &str, dark: bool) -> Option<String> {
    const MAX_FACETS: usize = 20_000;
    const W: f64 = 480.0;
    const H: f64 = 340.0;
    const PAD: f64 = 20.0;

    let vertices: Vec<V3> = src
        .lines()
        .filter_map(|l| {
            let mut t = l.split_whitespace();
            if !t.next()?.eq_ignore_ascii_case("vertex") {
                return None;
            }
            let mut c = t.filter_map(|v| v.parse::<f64>().ok().filter(|f| f.is_finite()));
            Some([c.next()?, c.next()?, c.next()?])
        })
        .collect();
    let facets = vertices.as_chunks::<3>().0;
    if facets.is_empty() || facets.len() > MAX_FACETS {
        return None;
    }

    // camera in front, to the right and above; light over its left shoulder
    let eye = unit([0.75, -1.0, 0.65]);
    let right = unit(cross([0.0, 0.0, 1.0], eye));
    let up = cross(eye, right);
    let light = unit([0.2, -0.9, 0.9]);
    let screen = |p: V3| [dot(p, right), -dot(p, up)];

    let (mut lo, mut hi) = ([f64::MAX; 2], [f64::MIN; 2]);
    for q in vertices.iter().map(|&p| screen(p)) {
        for i in 0..2 {
            lo[i] = lo[i].min(q[i]);
            hi[i] = hi[i].max(q[i]);
        }
    }
    let size = [(hi[0] - lo[0]).max(1e-9), (hi[1] - lo[1]).max(1e-9)];
    let scale = ((W - 2.0 * PAD) / size[0]).min((H - 2.0 * PAD) / size[1]);
    let offset = [
        (W - size[0] * scale) / 2.0 - lo[0] * scale,
        (H - size[1] * scale) / 2.0 - lo[1] * scale,
    ];

    // painter's algorithm: far facets first
    let depth = |f: &[V3; 3]| f.iter().map(|&p| dot(p, eye)).sum::<f64>();
    let mut order: Vec<&[V3; 3]> = facets.iter().collect();
    order.sort_by(|a, b| depth(a).total_cmp(&depth(b)));

    let base: V3 = if dark {
        [88.0, 166.0, 255.0]
    } else {
        [64.0, 120.0, 192.0]
    };
    let mut s = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}"><g stroke-width="0.6" stroke-linejoin="round">"#
    );
    for f in order {
        let normal = unit(cross(sub(f[1], f[0]), sub(f[2], f[0])));
        // two-sided lighting: models with inconsistent winding still look solid
        let shade = 0.3 + 0.7 * dot(normal, light).abs();
        let [r, g, b] = base.map(|c| (c * shade).round() as u8);
        let _ = write!(s, r#"<path d=""#);
        for (i, &p) in f.iter().enumerate() {
            let q = screen(p);
            let _ = write!(
                s,
                "{}{:.1} {:.1}",
                if i == 0 { 'M' } else { 'L' },
                q[0] * scale + offset[0],
                q[1] * scale + offset[1]
            );
        }
        let _ = write!(
            s,
            r##"Z" fill="#{r:02x}{g:02x}{b:02x}" stroke="#{r:02x}{g:02x}{b:02x}"/>"##
        );
    }
    s.push_str("</g></svg>");
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geojson_map() {
        let src = r#"{"type":"FeatureCollection","features":[{"type":"Feature","geometry":
            {"type":"Polygon","coordinates":[[[-90,35],[-90,30],[-85,30],[-85,35],[-90,35]]]}}]}"#;
        let svg = render("geojson", src, true).unwrap();
        assert!(svg.starts_with("<svg") && svg.contains("fill-opacity"));
        assert!(
            svg.contains("90°W") || svg.contains("88°W"),
            "graticule labels"
        );
        assert!(render("geojson", "{}", true).is_none());
        assert!(render("geojson", "not json", true).is_none());
    }

    #[test]
    fn topojson_arcs() {
        let v: Value = serde_json::from_str(
            r#"{"type":"Topology","transform":{"scale":[0.5,0.5],"translate":[100,0]},
            "objects":{"a":{"type":"GeometryCollection","geometries":[
              {"type":"Point","coordinates":[4,6]},
              {"type":"LineString","arcs":[0]},
              {"type":"Polygon","arcs":[[-1]]}]}},
            "arcs":[[[0,0],[2,2],[2,-2]]]}"#,
        )
        .unwrap();
        let mut g = Vec::new();
        Topology::new(&v).unwrap().objects(&v, &mut g);
        assert!(matches!(g[0], Geom::Point([102.0, 3.0])));
        let Geom::Line(l) = &g[1] else { panic!() };
        assert_eq!(l, &[[100.0, 0.0], [101.0, 1.0], [102.0, 0.0]]);
        let Geom::Polygon(p) = &g[2] else { panic!() };
        assert_eq!(p[0], [[102.0, 0.0], [101.0, 1.0], [100.0, 0.0]]);
    }

    #[test]
    fn base_map_loads() {
        assert!(countries().len() > 100);
    }

    #[test]
    fn stl_model() {
        let src = "solid t\n facet normal 0 0 1\n outer loop\n vertex 0 0 0\n vertex 1 0 0\n vertex 0 1 0\n endloop\n endfacet\nendsolid";
        let svg = render("stl", src, false).unwrap();
        assert_eq!(svg.matches("<path").count(), 1);
        assert!(render("stl", "solid empty\nendsolid", false).is_none());
    }

    #[test]
    #[cfg(feature = "mermaid")]
    fn mermaid_flowchart() {
        let svg = render("mermaid", "graph TD;\n A-->B;\n A-->C;", false).unwrap();
        assert!(svg.contains("<svg"));
    }
}
