//! The live preview: the app under development, restarted after each cycle.
//! Shared by `wish dev` and `wish lsp`.

use crate::engine;
use crate::project::Project;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ExitStatus};

#[derive(Default)]
pub struct Preview {
    child: Option<Child>,
}

impl Preview {
    /// (Re)start the app; each output line is passed to `on_line`.
    pub fn restart(&mut self, p: &Project, on_line: impl Fn(String) + Send + Clone + 'static) -> std::io::Result<()> {
        self.stop();
        if !p.preview_enabled() {
            return Ok(());
        }
        let mut c = engine::spawn_preview(p, true)?;
        let streams: Vec<Box<dyn Read + Send>> = [c.stdout.take().map(|s| Box::new(s) as Box<dyn Read + Send>), c.stderr.take().map(|s| Box::new(s) as Box<dyn Read + Send>)].into_iter().flatten().collect();
        for stream in streams {
            let f = on_line.clone();
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    f(line);
                }
            });
        }
        self.child = Some(c);
        Ok(())
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// If the app exited by itself, its status (once).
    pub fn exited(&mut self) -> Option<ExitStatus> {
        let status = self.child.as_mut()?.try_wait().ok().flatten()?;
        self.child = None;
        Some(status)
    }
}

impl Drop for Preview {
    fn drop(&mut self) {
        self.stop();
    }
}
