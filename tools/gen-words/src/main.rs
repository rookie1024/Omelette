extern crate dotenv;
extern crate regex;

#[macro_use]
extern crate diesel;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate lazy_static;

mod models;
mod schema;
mod thread_pool;

use regex::Regex;
use std::{
  collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
  env,
  fs::File,
  io::{self, prelude::*, BufReader},
  str,
  sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc::channel,
    Arc,
  },
  time::Instant,
};
use thread_pool::ThreadPool;

error_chain! {
  foreign_links {
    Diesel(diesel::result::Error);
    DieselConnection(diesel::ConnectionError);
    EnvVar(std::env::VarError);
    Io(io::Error);
  }

  errors {
    InvalidArg(expect: String) {
      description("invalid arguments"),
      display("invalid arguments: expected {}", expect),
    }

    ArgParse(msg: String) {
      description("argument parsing failed"),
      display("argument parsing failed: {}", msg),
    }
  }
}

#[derive(Clone)]
struct WordlistForm {
  blanked: String,
  full: String,
}

type CharCounts = HashMap<char, usize>;

fn count_chars(s: &str) -> CharCounts {
  let mut ret = CharCounts::new();

  for c in s.chars() {
    use std::collections::hash_map::Entry::*;

    match ret.entry(c) {
      Occupied(o) => {
        let val = o.into_mut();
        *val = *val + 1;
      }
      Vacant(v) => {
        v.insert(1);
      }
    }
  }

  ret
}

fn is_subseq(count: &CharCounts, of: &CharCounts) -> bool {
  count.iter().all(|(c, n)| n <= of.get(c).unwrap_or(&0))
}

static MIN_VALID_LEN: usize = 3;
static MIN_LEN: usize = 4;
static MAX_LEN: usize = 10;
static MAX_LEN_DIFFERENCE: usize = 5;

#[derive(Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
struct Normalized(String); // Used as a string with nonword characters stripped
#[derive(Clone, PartialEq, PartialOrd, Eq, Ord, Hash)]
struct Depermuted(String); // Used as a Normalized with its characters sorted

struct Stage1 {
  permutations: HashMap<Depermuted, HashSet<Normalized>>,
  counts: HashMap<Depermuted, CharCounts>,
  valid_subwords: HashSet<Depermuted>,
  len_groups: HashMap<usize, HashSet<Depermuted>>,
  forms: HashMap<Normalized, Vec<WordlistForm>>,
}

struct Stage2<'a> {
  sets: HashMap<Depermuted, Vec<Normalized>>, // TODO: can I go back to borrowing inside the vec?
  set_keys: HashMap<usize, Vec<&'a Depermuted>>,
  used_words: HashSet<Normalized>,
}

fn stage_1(file: &str, blacklist_file: &str) -> Result<Stage1> {
  let words: BTreeSet<_> = {
    let file = BufReader::new(File::open(file)?);

    file
      .lines()
      .map(|l| l.unwrap().trim().to_string())
      .collect()
  };

  println!("read {} word(s)", words.len());

  let blacklist: HashSet<_> = {
    let file = BufReader::new(File::open(blacklist_file)?);

    lazy_static! {
      static ref COMMENT_RE: Regex = Regex::new(r"^\s*#").unwrap();
    }

    file
      .lines()
      .map(|l| l.unwrap())
      .filter_map(|l| {
        if COMMENT_RE.is_match(&l) {
          None
        } else {
          Some(l.trim().to_string())
        }
      })
      .collect()
  };

  println!("read {} blacklisted word(s)", blacklist.len());

  let mut permutations: HashMap<Depermuted, HashSet<Normalized>> =
    HashMap::new();
  let mut counts: HashMap<Depermuted, CharCounts> = HashMap::new();
  let mut len_groups: HashMap<usize, HashSet<Depermuted>> = HashMap::new();
  let mut valid_subwords: HashSet<Depermuted> = HashSet::new();

  let mut forms: HashMap<Normalized, Vec<WordlistForm>> = HashMap::new();

  let mut used_blacklist: BTreeSet<Normalized> = BTreeSet::new();

  lazy_static! {
    static ref REJECT_RE: Regex = Regex::new(r"[\d\s]").unwrap();
    static ref NORMAL_RE: Regex = Regex::new(r"\W+").unwrap();
    static ref BLANK_RE: Regex = Regex::new(r"[\w--\p{Lu}\p{Lt}]").unwrap();
    static ref BLANK_CAPS_RE: Regex = Regex::new(r"[\p{Lu}\p{Lt}]").unwrap();
  }

  let blacklist: HashSet<_> = blacklist
    .iter()
    .map(|w| {
      let lower = w.to_lowercase();
      Normalized(NORMAL_RE.replace_all(&lower, "").into_owned())
    })
    .collect();

  for word in words {
    use std::collections::hash_map::Entry::*;

    if REJECT_RE.is_match(&word) {
      continue;
    }

    let normalized = word.to_lowercase();
    let normalized =
      Normalized(NORMAL_RE.replace_all(&normalized, "").into_owned());

    if blacklist.contains(&normalized) {
      used_blacklist.insert(normalized.clone());
      continue;
    }

    let blank = BLANK_RE.replace_all(&word, "_");
    let blank = BLANK_CAPS_RE.replace_all(&blank, "_").into_owned(); // TODO: highlight this somehow?

    match forms.entry(normalized.clone()) {
      Vacant(v) => v.insert(Vec::new()),
      Occupied(o) => o.into_mut(),
    }.push(WordlistForm {
      blanked: blank,
      full: word.clone(),
    });

    let mut depermuted: Vec<_> = normalized.0.chars().collect();
    depermuted.sort();
    let depermuted = Depermuted(depermuted.into_iter().collect());

    match permutations.entry(depermuted.clone()) {
      Vacant(v) => {
        v.insert(HashSet::new()).insert(normalized);
        counts.insert(depermuted.clone(), count_chars(&depermuted.0));

        match len_groups.entry(depermuted.0.len()) {
          Vacant(v) => {
            v.insert(HashSet::new()).insert(depermuted.clone());
          }
          Occupied(o) => {
            o.into_mut().insert(depermuted.clone());
          }
        }

        if depermuted.0.len() >= MIN_VALID_LEN {
          valid_subwords.insert(depermuted);
        }
      }
      Occupied(o) => {
        o.into_mut().insert(normalized);
      }
    }
  }

  println!("{} normalized", forms.len());
  println!("{} depermuted", permutations.len());
  println!("{} valid subword(s)", valid_subwords.len());

  {
    let used = used_blacklist;
    let unused: BTreeSet<_> =
      blacklist.iter().filter(|w| !used.contains(w)).collect();

    println!("performing extra blacklist checks...");

    let mut maybe: BTreeMap<&Normalized, BTreeSet<&String>> = BTreeMap::new();

    for (p, fs) in forms.iter().filter_map(|(n, fs)| {
      if used.contains(n) {
        return None;
      }

      if let Some(p) = blacklist.iter().find(|p| n.0.contains(&p.0)) {
        Some((p, fs))
      } else {
        None
      }
    }) {
      use std::collections::btree_map::Entry::*;

      let mut set = match maybe.entry(p) {
        Vacant(v) => v.insert(BTreeSet::new()),
        Occupied(o) => o.into_mut(),
      };

      for f in fs {
        set.insert(&f.full);
      }
    }

    println!(
      "{} blacklisted used, {} unused",
      used.len(),
      unused.len()
    );

    {
      let mut file = File::create("usblk.log")?;

      for word in used {
        writeln!(file, "{}", word.0)?;
      }
    }

    {
      let mut file = File::create("unblk.log")?;

      for word in unused {
        writeln!(file, "{}", word.0)?;
      }
    }

    {
      let mut file = File::create("maybeblk.log")?;

      for (blk, words) in maybe {
        writeln!(file, "{}: ({})", blk.0, words.len())?;

        for word in words {
          writeln!(file, "  {}", word)?;
        }
      }
    }
  }

  Ok(Stage1 {
    permutations,
    counts,
    valid_subwords,
    len_groups,
    forms,
  })
}

fn stage_2<'a>(s1: &'a Arc<Stage1>) -> Result<Stage2<'a>> {
  let mut sets: HashMap<Depermuted, Vec<Normalized>> = HashMap::new(); // TODO: can I go back to borrowing inside the vec?
  let mut set_keys: HashMap<usize, Vec<&Depermuted>> = HashMap::new();

  let mut used_words: HashSet<Normalized> = HashSet::new();

  let (set_tx, set_rx) = channel();

  let done = Arc::new(AtomicUsize::new(0));
  let total = Arc::new(AtomicUsize::new(0));

  let start = Instant::now();

  let worker: ThreadPool<_> = ThreadPool::new(
    (0..10)
      .map(|_| (Arc::clone(s1), done.clone(), total.clone(), set_tx.clone()))
      .collect(),
    |_id,
     (s1, done, total, set_tx),
     (depermuted, count): (Depermuted, CharCounts)| {
      let i = done.fetch_add(1, Ordering::Relaxed);
      if i % 10 == 0 {
        print!(
          "\r\x1b[2K({}/{}) {}",
          i,
          total.load(Ordering::Relaxed),
          &depermuted.0
        );
        io::stdout().flush().unwrap();
      }

      let mut list: Vec<_> = s1
        .valid_subwords
        .iter()
        .filter(|deperm2| {
          deperm2.0.len() <= depermuted.0.len()
            && (depermuted.0.len() < MAX_LEN_DIFFERENCE
              || deperm2.0.len() >= depermuted.0.len() - MAX_LEN_DIFFERENCE)
            && is_subseq(&s1.counts[*deperm2], &count)
        })
        .flat_map(|d| s1.permutations[d].clone()) // TODO: can I go back to borrowing this?
        .collect();

      list.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then(a.0.cmp(&b.0)));

      set_tx
        .send((depermuted, list))
        .expect("failed to send result");
    },
  );

  for len in MIN_LEN..MAX_LEN + 1 {
    let mut keys: Vec<&Depermuted> = Vec::new();

    total.fetch_add(s1.len_groups[&len].len(), Ordering::Relaxed);

    for (_, depermuted) in s1.len_groups[&len].iter().enumerate() {
      worker.queue((depermuted.clone(), s1.counts[depermuted].clone()));
      keys.push(depermuted);
    }

    set_keys.insert(len, keys);
  }

  worker.join();

  let end = Instant::now();
  let time = end - start;

  println!(
    "\r\x1b[2K{} processed in {}.{:02}s",
    done.load(Ordering::Acquire),
    time.as_secs(),
    time.subsec_millis() / 10
  );

  for (depermuted, list) in set_rx.try_iter() {
    for norm in &list {
      used_words.insert(Normalized::clone(norm));
    }

    sets.insert(depermuted, list);
  }

  Ok(Stage2 {
    sets,
    set_keys,
    used_words,
  })
}

fn run() -> Result<()> {
  let mut args: VecDeque<_> = env::args().collect();
  args.pop_front(); // drop argv[0]

  fn parse_arg<T>(args: &mut VecDeque<String>, expect: &str) -> Result<T>
  where
    T: std::str::FromStr,
    <T as std::str::FromStr>::Err: std::string::ToString,
  {
    match args.pop_front() {
      Some(a) => a,
      None => return Err(ErrorKind::InvalidArg(expect.into()).into()),
    }.parse()
      .map_err(|e: <T as std::str::FromStr>::Err| {
        ErrorKind::ArgParse(e.to_string()).into()
      })
  }

  let file: String = parse_arg(&mut args, "an input filename")?;

  let s1 = Arc::new(stage_1(&file, "etc/blacklist.txt")?);

  let s2 = stage_2(&s1)?;

  let mut forms = s1.forms.clone();

  forms.retain(|k, _| s2.used_words.contains(k));

  {
    use diesel::{insert_into, prelude::*, sqlite::SqliteConnection};
    use dotenv::dotenv;
    use models::*;

    println!("collecting models...");

    let mut insert_form_ids: Vec<FormId> = Vec::new();
    let mut insert_forms: Vec<Form> = Vec::new();
    let mut insert_set_ids: Vec<SetId> = Vec::new();
    let mut insert_sets: Vec<Set> = Vec::new();
    let mut insert_set_keys: Vec<SetKey> = Vec::new();

    for (i, (norm, forms)) in forms.iter().enumerate() {
      insert_form_ids.push(FormId {
        norm: &norm.0,
        id: i as i32,
      });

      for form in forms {
        let oid = insert_forms.len() as i32;
        insert_forms.push(Form {
          oid,
          id: i as i32,
          blank: &form.blanked,
          full: &form.full,
        });
      }
    }

    for (i, (deperm, norms)) in s2.sets.iter().enumerate() {
      insert_set_ids.push(SetId {
        key: &deperm.0,
        id: i as i32,
      });

      for norm in norms {
        let oid = insert_sets.len() as i32;
        insert_sets.push(Set {
          oid,
          id: i as i32,
          norm: &norm.0,
        });
      }
    }

    for (len, deperms) in &s2.set_keys {
      for deperm in deperms {
        let oid = insert_set_keys.len() as i32;
        insert_set_keys.push(SetKey {
          oid,
          len: *len as i32,
          key: &deperm.0,
        });
      }
    }

    println!("committing to database...");

    dotenv().ok();

    let url = env::var("DATABASE_URL")?;
    let conn = SqliteConnection::establish(&url)?;

    let start = Instant::now();

    {
      use schema::{
        form_ids::dsl::*, forms::dsl::*, set_ids::dsl::*, set_keys::dsl::*,
        sets::dsl::*,
      };

      println!("  form_ids");
      insert_into(form_ids)
        .values(&insert_form_ids)
        .execute(&conn)?;

      println!("  forms");
      insert_into(forms).values(&insert_forms).execute(&conn)?;

      println!("  set_ids");
      insert_into(set_ids).values(&insert_set_ids).execute(&conn)?;

      println!("  sets");
      insert_into(sets).values(&insert_sets).execute(&conn)?;

      println!("  set_keys");
      insert_into(set_keys)
        .values(&insert_set_keys)
        .execute(&conn)?;
    }

    let end = Instant::now();
    let time = end - start;

    println!(
      "committed in {}.{:02}s",
      time.as_secs(),
      time.subsec_millis() / 10
    );
  }

  Ok(())
}

fn main() {
  match run() {
    Ok(_) => return,
    Err(e) => writeln!(io::stderr(), "an error occurred: {}", e).unwrap(),
  }
}
