extern crate ncurses as nc;
extern crate rand;
extern crate regex;

#[macro_use]
extern crate diesel;
#[macro_use]
extern crate lazy_static;

// TODO: move the models and schema modules into the word_list module
mod markov;
mod models;
mod schema;
mod tui;
mod word_list;

use rand::prelude::*;
use std::{
  collections::{HashMap, HashSet},
  fs::File,
  io::{self, prelude::*},
  panic,
};
use tui::{
  controls::*,
  element::{self as el, Element},
};
use word_list::WordList;

fn dump_line(win: nc::WINDOW, y: i32, line: &str) {
  nc::wmove(win, y, 0);
  nc::wclrtoeol(win);
  nc::mvwaddstr(win, y, 0, line);
  nc::wrefresh(win);
}

fn main() {
  panic::catch_unwind(|| {
    nc::endwin();
  }).unwrap();

  let words = WordList::new("etc/words.sqlite3");

  let mut len: Option<usize> = None;

  'main: loop {
    let key;
    let set = {
      let mut keys = loop {
        if let None = len {
          let mut len_str = String::new();

          write!(io::stderr(), "word length: ").unwrap();
          io::stderr().flush().unwrap();

          if io::stdin().read_line(&mut len_str).unwrap() == 0 {
            writeln!(io::stderr(), "").unwrap();
            return;
          }

          len = Some(match len_str.trim().parse() {
            Ok(l) => l,
            Err(e) => {
              writeln!(io::stderr(), "invalid number: {}", e).unwrap();
              continue;
            }
          });
        }

        let _len = len.unwrap();

        let keys = words.get_set_keys(&_len);

        match keys.len() {
          0 => {
            writeln!(io::stderr(), "no words found of length {}", _len)
              .unwrap();
            len = None;
            continue;
          }
          _ => break keys,
        }
      };

      let nkeys = keys.len();

      key = keys.remove(rand::thread_rng().gen_range(0, nkeys));

      words.get_set(&key)
    };

    let markov = {
      use std::collections::hash_map::Entry::*;

      let mut table = markov::analyze_corpus(
        set.iter().map(|s| ((s.len() as f64).powf(3.4), s.chars())),
      );
      let chars: HashSet<_> = set.iter().flat_map(|s| s.chars()).collect();

      let pad = table
        .values()
        .flat_map(|t| t.values())
        .fold(0.0, |s, c| s + c) / 100.0;

      for chr in &chars {
        let tos = match table.entry(*chr) {
          Vacant(v) => v.insert(HashMap::new()),
          Occupied(o) => o.into_mut(),
        };

        for chr in &chars {
          match tos.entry(*chr) {
            Vacant(v) => {
              v.insert(pad);
            }
            Occupied(o) => {
              let o = o.into_mut();
              *o = *o + pad;
            }
          }
        }
      }

      let mut file = File::create("freq.log").unwrap();

      writeln!(file, "table:").unwrap();

      for (from, tos) in &table {
        for (to, freq) in tos {
          writeln!(file, "  {} -> {}: {}", from, to, freq).unwrap();
        }
      }

      let markov = markov::Markov::new(table);

      writeln!(file, "samples:").unwrap();

      for s in markov.rand_seed().take(20) {
        let line: String = markov.iter(s).take(40).collect();

        writeln!(file, "{}", line).unwrap();
      }

      markov
    };

    let mut remain: HashSet<&String> = set.iter().collect();

    let win = nc::initscr();
    nc::start_color();
    nc::cbreak();
    nc::noecho();
    nc::keypad(win, true);

    let ghost_pair: i32 = 1;
    nc::init_pair(ghost_pair as i16, 2, 0);
    // nc::init_extended_pair(ghost_pair, 2, 0);

    let bad_ghost_pair: i32 = 2;
    nc::init_pair(bad_ghost_pair as i16, 1, 0);

    let auto_ghost_pair: i32 = 3;
    nc::init_pair(auto_ghost_pair as i16, 3, 0);

    let reveal_pair: i32 = 4;
    nc::init_pair(reveal_pair as i16, 3, 0);

    let hl_pair: i32 = 5;
    nc::init_pair(hl_pair as i16, 2, 0);

    let word_box = el::wrap(WordBox::new(
      key.clone(),
      ghost_pair,
      bad_ghost_pair,
      auto_ghost_pair,
    ));

    let mut match_boxes: HashMap<&String, Vec<_>> = HashMap::new();

    for norm in &set {
      let forms = words.get_form(norm);

      match_boxes.insert(
        norm,
        forms
          .into_iter()
          .map(|form| el::wrap(MatchBox::new(form, reveal_pair, hl_pair)))
          .collect(),
      );
    }

    let match_box_panel = el::wrap(WrapBox::new(
      set
        .iter()
        .flat_map(|i| &match_boxes[i])
        .map(|b| el::add_ref(b)),
      WrapMode::Cols,
      WrapAlign::Begin,
      3,
    ));

    let mut hl_match_boxes: Option<&Vec<el::ElemWrapper<MatchBox>>> = None;

    let center_test = el::wrap(TestView::new(
      el::add_ref(&word_box),
      el::add_ref(&match_box_panel),
    ));

    let ui_root = UiRoot::new(win, el::add_ref(&center_test));

    ui_root.resize();

    while remain.len() > 0 {
      // TODO: handle modifier keys better
      // TODO: up and down should be history controls, not text editing controls
      match nc::wgetch(win) {
        0x04 => break 'main,
        0x09 => word_box.borrow_mut().shuffle(&markov), // HT
        0x17 => word_box.borrow_mut().clear(),          // ETB (ctrl+bksp)
        0x1B => {
          // ESC

          if let Some(b) = hl_match_boxes {
            for b in b {
              let mut b = b.borrow_mut();

              b.set_style(MatchBoxStyle::Normal);
            }
          }

          for boxes in match_boxes.values() {
            for match_box in boxes {
              let mut match_box = match_box.borrow_mut();

              if !match_box.revealed() {
                match_box.set_revealed(true);
                match_box.set_style(MatchBoxStyle::Reveal);
              }
            }
          }

          word_box.borrow_mut().render_cur();

          match nc::wgetch(win) {
            0x04 => break 'main,
            _ => {}
          }

          len = None;
          break;
        }
        0x0A => {
          // EOL
          if let Some(b) = hl_match_boxes {
            for b in b {
              let mut b = b.borrow_mut();

              b.set_style(MatchBoxStyle::Normal);
            }
          }

          {
            let mut word_box = word_box.borrow_mut();

            remain.remove(word_box.buf());

            let success = match match_boxes.get(word_box.buf()) {
              Some(b) => {
                hl_match_boxes = Some(b);
                word_box.set_bad(false);

                let mut success = true;

                for b in b {
                  let mut b_ref = b.borrow_mut();

                  if b_ref.revealed() {
                    b_ref.set_style(MatchBoxStyle::Highlight);
                    success = false;
                  } else {
                    b_ref.set_revealed(true);
                    b_ref.set_style(MatchBoxStyle::Reveal);
                  }
                }

                success
              }
              None => {
                let bad = word_box.buf().len() > 0;
                word_box.set_bad(bad);
                false
              }
            };

            if success {
              word_box.set_auto_sort(false);
              word_box.clear();
            } else if word_box.auto_sort() {
              word_box.render_cur();
            } else {
              word_box.clear();
            }
          }
        }
        0x7F => word_box.borrow_mut().del_left(), // DEL (bksp)
        nc::KEY_DOWN => word_box.borrow_mut().end(),
        nc::KEY_UP => word_box.borrow_mut().home(),
        nc::KEY_LEFT => word_box.borrow_mut().left(),
        nc::KEY_RIGHT => word_box.borrow_mut().right(),
        nc::KEY_HOME => word_box.borrow_mut().home(),
        nc::KEY_BACKSPACE => word_box.borrow_mut().clear(), // (shift+bksp/ctrl+bksp)
        nc::KEY_DC => word_box.borrow_mut().del_right(),
        nc::KEY_BTAB => {
          let mut word_box = word_box.borrow_mut();
          let val = !word_box.auto_sort();
          word_box.set_auto_sort(val);
        } // (shift+tab)
        nc::KEY_END => word_box.borrow_mut().end(),
        nc::KEY_RESIZE => ui_root.resize(),
        0o1051 => word_box.borrow_mut().home(), // ctrl+left somehow?
        0o1070 => word_box.borrow_mut().end(),  // ctrl+right somehow?
        ch => {
          let mut word_box = word_box.borrow_mut();

          if ch < nc::KEY_MIN {
            let ch = ch as u8 as char;

            if !ch.is_control() {
              let s = ch.to_lowercase().to_string();
              word_box.put(&s);
            } else {
              // dump_line(win, 3, &ch.escape_unicode().to_string());
              // word_box.render_cur();
            }
          } else {
            // dump_line(win, 4, &ch.to_string());
            // word_box.render_cur();
          }
        }
      }
    }

    nc::endwin();
  }

  nc::endwin();
}
