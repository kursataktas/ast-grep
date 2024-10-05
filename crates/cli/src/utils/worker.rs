use crate::utils::FileStats;

use anyhow::{anyhow, Result};
use ignore::{DirEntry, WalkParallel, WalkState};

use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};

/// A trait to abstract how ast-grep discovers work Items.
///
/// It follows multiple-producer-single-consumer pattern.
/// ast-grep will produce items in one or more separate thread(s) and
/// `consume_items` in the main thread, blocking the function return.
/// Worker at the moment has two main flavors:
/// * PathWorker: discovers files on the file system, based on ignore
/// * StdInWorker: parse text content from standard input stream
pub trait Worker: Sync + Send {
  /// The item to send between producer/consumer threads.
  /// It is usually parsed tree-sitter Root with optional data.
  type Item: Send + 'static;
  /// `consume_items` will run in a separate single thread.
  /// printing matches or error reporting can happen here.
  fn consume_items(&self, items: Items<Self::Item>) -> Result<()>;
}

/// A trait to abstract how ast-grep discovers, parses and processes files.
///
/// It follows multiple-producer-single-consumer pattern.
/// ast-grep discovers files in parallel by `build_walk`.
/// Then every file is parsed and filtered in `produce_item`.
/// Finally, `produce_item` will send `Item` to the consumer thread.
pub trait PathWorker: Worker {
  /// WalkParallel will determine what files will be processed.
  fn build_walk(&self) -> Result<WalkParallel>;
  /// Record stats for the worker.
  fn get_stats(&self) -> &FileStats;
  /// Parse and find_match can be done in `produce_item`.
  fn produce_item(&self, path: &Path) -> Option<Vec<Self::Item>>;

  fn run_path(self) -> Result<()>
  where
    Self: Sized + 'static,
  {
    run_worker(Arc::new(self))
  }
}

pub trait StdInWorker: Worker {
  fn parse_stdin(&self, src: String) -> Option<Self::Item>;

  fn run_std_in(&self) -> Result<()> {
    let source = std::io::read_to_string(std::io::stdin())?;
    if let Some(item) = self.parse_stdin(source) {
      self.consume_items(Items::once(item)?)
    } else {
      Ok(())
    }
  }
}

pub struct Items<T>(mpsc::Receiver<T>);
impl<T> Iterator for Items<T> {
  type Item = T;
  fn next(&mut self) -> Option<Self::Item> {
    if let Ok(match_result) = self.0.recv() {
      Some(match_result)
    } else {
      None
    }
  }
}
impl<T> Items<T> {
  fn once(t: T) -> Result<Self> {
    let (tx, rx) = mpsc::channel();
    // use write to avoid send/sync trait bound
    match tx.send(t) {
      Ok(_) => (),
      Err(e) => return Err(anyhow!(e.to_string())),
    };
    Ok(Items(rx))
  }
}

fn filter_result(result: Result<DirEntry, ignore::Error>) -> Option<PathBuf> {
  let entry = match result {
    Ok(entry) => entry,
    Err(err) => {
      eprintln!("ERROR: {}", err);
      return None;
    }
  };
  if !entry.file_type()?.is_file() {
    return None;
  }
  let path = entry.into_path();
  // TODO: is it correct here? see https://github.com/ast-grep/ast-grep/issues/1343
  match path.strip_prefix("./") {
    Ok(p) => Some(p.to_path_buf()),
    Err(_) => Some(path),
  }
}

fn run_worker<W: PathWorker + ?Sized + 'static>(worker: Arc<W>) -> Result<()> {
  let (tx, rx) = mpsc::channel();
  let w = worker.clone();
  let walker = worker.build_walk()?;
  // walker run will block the thread
  std::thread::spawn(move || {
    let tx = tx;
    walker.run(|| {
      let tx = tx.clone();
      let w = w.clone();
      Box::new(move |result| {
        let Some(p) = filter_result(result) else {
          return WalkState::Continue;
        };
        let stats = w.get_stats();
        stats.add_scanned();
        let Some(items) = w.produce_item(&p) else {
          stats.add_skipped();
          return WalkState::Continue;
        };
        for result in items {
          match tx.send(result) {
            Ok(_) => continue,
            Err(_) => return WalkState::Quit,
          }
        }
        WalkState::Continue
      })
    });
  });
  worker.consume_items(Items(rx))
}