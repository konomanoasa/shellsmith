use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Fixture {
  pub root: PathBuf,
}
impl Fixture {
  pub fn new() -> Self {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("target/application-tests")
      .join(format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
      ));
    std::fs::create_dir_all(&root).unwrap();
    Self { root }
  }
  pub fn write(&self, name: &str, source: &str) -> PathBuf {
    let path = self.root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, source).unwrap();
    path
  }
}
impl Drop for Fixture {
  fn drop(&mut self) {
    let _ = std::fs::remove_dir_all(&self.root);
  }
}
