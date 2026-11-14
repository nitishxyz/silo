//! Find in page: `Browser::find` asks Chromium, `Event::Find` reports the
//! matches as they are counted.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::os::raw::c_int;
use std::sync::Arc;

use cef::rc::Rc as _;
use cef::*;

use crate::shared::{Event, Shared};

/// Matches of the current search.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FindResult {
    /// Matches counted so far.
    pub count: u32,
    /// The selected match, from 1; 0 when none is.
    pub active: u32,
    /// The count is complete.
    pub done: bool,
}

wrap_find_handler! {
    pub(crate) struct PageFind {
        shared: Arc<Shared>,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            _browser: Option<&mut Browser>,
            _identifier: c_int,
            count: c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: c_int,
            final_update: c_int,
        ) {
            let result = FindResult {
                count: u32::try_from(count).unwrap_or(0),
                active: u32::try_from(active_match_ordinal).unwrap_or(0),
                done: final_update != 0,
            };
            self.shared.push(Event::Find(result));
        }
    }
}

pub(crate) fn handler(shared: &Arc<Shared>) -> FindHandler {
    PageFind::new(Arc::clone(shared))
}
