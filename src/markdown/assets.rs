//! Pictures a document refers to (images, rendered diagrams) and the caches
//! that make laying the same document out again cheap.
//!
//! Pictures load on background threads. A layout waits briefly for local
//! files and built-in diagrams, which take milliseconds; downloads and `mmdc`
//! runs take seconds, so the document is shown without them first and laid
//! out again as they arrive.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::diagram;
use crate::fetch;
use crate::raster::{Picture, Raster};
use crate::ui::Span;

type Pic = Option<Arc<Picture>>;

/// Something to load, identified by `key()`.
pub enum Job {
    /// Image at a path or URL.
    Image(String),
    /// Diagram code block.
    Diagram {
        lang: String,
        src: String,
        dark: bool,
    },
}

pub fn diagram_key(lang: &str, src: &str, dark: bool) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (lang, src, dark).hash(&mut h);
    format!("{lang}:{:x}", h.finish())
}

impl Job {
    fn key(&self) -> String {
        match self {
            Job::Image(k) => k.clone(),
            Job::Diagram { lang, src, dark } => diagram_key(lang, src, *dark),
        }
    }

    /// Usually done within milliseconds: worth holding the first frame for.
    fn quick(&self) -> bool {
        !matches!(self, Job::Image(k) if fetch::is_url(k))
    }

    /// Load the picture. A crashing decoder counts as an unreadable image.
    fn run(&self) -> Pic {
        let load = || match self {
            Job::Image(k) if fetch::is_url(k) => {
                crate::raster::decode(&fetch::fetch_untrusted(k).ok()?, k, None).ok()
            }
            Job::Image(k) => {
                crate::raster::decode(&fetch::read_source(k).ok()?, k, Path::new(k).parent()).ok()
            }
            Job::Diagram { lang, src, dark } => match diagram::render(lang, src, *dark) {
                Some(svg) => crate::raster::decode_generated_svg(&svg).ok(),
                // not drawn by the built-in renderer: try mermaid-cli
                None if lang == "mermaid" => mmdc(src, *dark),
                None => None,
            },
        };
        crate::app::quiet_panics(load).flatten().map(Arc::new)
    }
}

/// Heading lines typeset as images: (raster, text).
pub type Typeset = Arc<Vec<(Arc<Raster>, String)>>;

pub struct Assets {
    pics: HashMap<String, Pic>,
    /// Keys of jobs still running.
    loading: HashSet<String>,
    tx: Sender<(String, Pic)>,
    rx: Receiver<(String, Pic)>,
    // Results of the expensive layout steps, valid for one geometry and theme.
    valid_for: String,
    pub scaled: HashMap<(String, u32, u32), Arc<Raster>>,
    pub headings: HashMap<String, Option<Typeset>>,
    pub code: HashMap<u64, Arc<Vec<Vec<Span>>>>,
}

impl Default for Assets {
    fn default() -> Self {
        let (tx, rx) = channel();
        Assets {
            pics: HashMap::new(),
            loading: HashSet::new(),
            tx,
            rx,
            valid_for: String::new(),
            scaled: HashMap::new(),
            headings: HashMap::new(),
            code: HashMap::new(),
        }
    }
}

impl Assets {
    pub fn get(&self, key: &str) -> Pic {
        self.pics.get(key).cloned().flatten()
    }

    /// Number of pictures still being loaded in the background.
    pub fn loading(&self) -> usize {
        self.loading.len()
    }

    /// Drop the layout caches unless they were filled for `geometry`.
    pub fn validate(&mut self, geometry: String) {
        if self.valid_for != geometry {
            self.valid_for = geometry;
            self.scaled.clear();
            self.headings.clear();
            self.code.clear();
        }
    }

    /// Take over finished background jobs; true if there were any.
    pub fn poll(&mut self) -> bool {
        let mut any = false;
        while let Ok(done) = self.rx.try_recv() {
            self.finish(done);
            any = true;
        }
        any
    }

    fn finish(&mut self, (key, pic): (String, Pic)) {
        self.loading.remove(&key);
        self.pics.insert(key, pic);
    }

    /// Start loading what is not known yet, and wait for it: for everything
    /// if `wait` is set, otherwise shortly for the quick jobs only. The rest
    /// arrives through `poll`.
    pub fn load(&mut self, jobs: Vec<Job>, wait: bool) {
        const PATIENCE: Duration = Duration::from_millis(80);
        let mut jobs: Vec<Job> = jobs
            .into_iter()
            .filter(|j| {
                let k = j.key();
                !self.pics.contains_key(&k) && self.loading.insert(k)
            })
            .collect();
        if jobs.is_empty() {
            return;
        }
        // SVG images with text need the system fonts: index them meanwhile
        let maybe_svg = |j: &Job| matches!(j, Job::Image(k) if fetch::is_url(k) || k.to_ascii_lowercase().contains(".svg"));
        if jobs.iter().any(maybe_svg) {
            crate::raster::warm_fonts();
        }

        // workers take jobs from the end of the queue: quick ones first
        jobs.sort_by_key(Job::quick);
        let mut quick: HashSet<String> = jobs.iter().filter(|j| j.quick()).map(Job::key).collect();
        let workers = jobs.len().min(16);
        let queue = Arc::new(Mutex::new(jobs));
        for _ in 0..workers {
            let (queue, tx) = (queue.clone(), self.tx.clone());
            std::thread::spawn(move || {
                while let Some(job) = queue.lock().ok().and_then(|mut q| q.pop()) {
                    if tx.send((job.key(), job.run())).is_err() {
                        break;
                    }
                }
            });
        }

        let deadline = Instant::now() + PATIENCE;
        loop {
            let done = if wait && !self.loading.is_empty() {
                self.rx.recv().ok()
            } else if !wait && !quick.is_empty() {
                let left = deadline.saturating_duration_since(Instant::now());
                self.rx.recv_timeout(left).ok()
            } else {
                None
            };
            let Some(done) = done else {
                break;
            };
            quick.remove(&done.0);
            self.finish(done);
        }
    }
}

/// Render a mermaid diagram with mermaid-cli.
fn mmdc(src: &str, dark: bool) -> Option<Picture> {
    use std::os::unix::fs::DirBuilderExt;
    use std::process::{Command, Stdio};
    let mmdc = fetch::tool("mmdc")?;
    // private, unpredictable working directory, removed afterwards
    let dir = std::env::temp_dir().join(format!("shw-mermaid-{}", crate::kitty::random_hex(12)));
    std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    let (input, output) = (dir.join("diagram.mmd"), dir.join("diagram.png"));
    let run = || {
        crate::kitty::write_private_new(&input, src.as_bytes()).ok()?;
        // Run inside the private directory: Puppeteer (used by mmdc) loads config
        // files such as .puppeteerrc.cjs from the working directory.
        let status = Command::new(&mmdc)
            .current_dir(&dir)
            .arg("-i")
            .arg(&input)
            .arg("-o")
            .arg(&output)
            .args(["-b", "transparent", "-s", "2", "-q", "-t"])
            .arg(if dark { "dark" } else { "default" })
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .ok()?;
        let data = std::fs::read(&output).ok().filter(|_| status.success())?;
        let mut pic = crate::raster::decode(&data, "diagram.png", None).ok()?;
        // rendered at scale 2: report CSS size
        pic.width = (pic.width / 2).max(1);
        pic.height = (pic.height / 2).max(1);
        Some(pic)
    };
    let pic = run();
    let _ = std::fs::remove_dir_all(&dir);
    pic
}
