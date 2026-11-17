// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::BTreeMap;

use super::ordered_collection::OrderedCollection;

#[derive(Debug, Clone, PartialEq)]
enum Status {
    Pending,
    Ready,
}

pub struct UniquePriorityQueue<P, K, V> {
    priority: BTreeMap<P, (Status, K, V)>,
    key_to_priority: BTreeMap<K, P>,
}

impl<P: Clone + Ord, K: Clone + Ord, V> UniquePriorityQueue<P, K, V> {
    pub fn new() -> UniquePriorityQueue<P, K, V> {
        UniquePriorityQueue {
            priority: BTreeMap::new(),
            key_to_priority: BTreeMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.priority.len()
    }

    pub fn is_empty(&self) -> bool {
        self.priority.is_empty()
    }

    pub fn find_key(&self, k: &K) -> Option<&P> {
        self.key_to_priority.get(k)
    }

    /// Insert `(p, k, v)`. Returns `false` if priority or key already exists.
    pub fn put_value(&mut self, p: P, k: K, v: V) -> bool {
        if self.priority.contains_key(&p) || self.key_to_priority.contains_key(&k) {
            return false;
        }
        self.priority
            .insert_unique(p.clone(), (Status::Pending, k.clone(), v));
        self.key_to_priority.insert(k, p);
        true
    }

    pub fn find_value_of_key(&self, k: &K) -> Option<&V> {
        let prio_opt = self.key_to_priority.get(k);
        prio_opt.and_then(|prio| self.priority.get(prio).map(|(_, _, v_opt)| v_opt))
    }

    pub fn find_mut_value_of_priority(&mut self, p: &P) -> Option<&mut V> {
        self.priority
            .get_mut(p)
            .map(|&mut (_, _, ref mut v_opt)| v_opt)
    }

    pub fn update_value<F>(&mut self, k: &K, f: F)
    where
        F: FnOnce(&mut V),
    {
        let prio = self.key_to_priority
            .get(k)
            .expect("update_value: Key must exist.");
        let cur = self.priority
            .get_mut(prio)
            .expect("update_value: Priority must exist.");
        f(&mut cur.2);
    }

    pub fn set_ready(&mut self, p: &P) {
        let cur = self.priority
            .get_mut(p)
            .expect("set_ready: Priority must exist.");
        cur.0 = Status::Ready;
    }

    pub fn pop_min_if_complete(&mut self) -> Option<(P, K, V)> {
        let min_opt = self.priority.pop_min_when(|_k, min| min.0 == Status::Ready);
        min_opt.map(|(p, (_status, k, v))| {
            self.key_to_priority.remove(&k);
            (p, k, v)
        })
    }
}

impl<P: Clone + Ord, K: Clone + Ord, V> Default for UniquePriorityQueue<P, K, V> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use quickcheck;

    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn insert1() {
        fn prop(priority: i8, key: isize, value: i8) -> bool {
            let mut upq = UniquePriorityQueue::new();
            assert!(upq.put_value(priority, key, value));
            assert_eq!(upq.pop_min_if_complete(), None);
            upq.set_ready(&priority);
            assert_eq!(upq.pop_min_if_complete(), Some((priority, key, value)));

            true
        }
        quickcheck::quickcheck(prop as fn(i8, isize, i8) -> bool);
    }

    #[test]
    fn insert_many() {
        fn prop(keys: Vec<(i8, isize, i8)>) -> bool {
            let mut upq = UniquePriorityQueue::new();
            assert_eq!(upq.pop_min_if_complete(), None);

            // Insert priorities
            let mut in_use = BTreeMap::new();
            for (p, k, v) in keys.iter() {
                if upq.put_value(*p, *k, *v) {
                    in_use.insert(*p, (*k, *v));
                    assert_eq!(upq.find_key(k), Some(p));
                    assert_eq!(upq.find_value_of_key(k), Some(v));
                }
            }
            assert_eq!(upq.pop_min_if_complete(), None);

            // Commit all
            for p in in_use.keys() {
                upq.set_ready(p);
            }

            // Verify that everything is now there, and all entries are complete
            for _ in 0..in_use.len() {
                let next = upq.pop_min_if_complete();
                assert!(next.is_some());

                let (p, k, v) = next.unwrap();
                assert_eq!(in_use.get(&p), Some(&(k, v)));
            }

            assert_eq!(upq.pop_min_if_complete(), None);

            true
        }

        quickcheck::quickcheck(prop as fn(Vec<(i8, isize, i8)>) -> bool);
    }
}
