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

use quickcheck;
use super::fs::FileReader;

#[test]
fn filereader() {
    fn prop(data: Vec<Vec<u8>>, offset: u16, size: u8) -> bool {
        let offset: usize = offset as usize;
        let size: usize = size.into();

        let reference: Vec<u8> = data.iter().flat_map(|v| v.iter()).cloned().collect();

        let mut reader = FileReader::new_from_iter(Some(Box::new(data.into_iter())));

        if let Some(slice) = reader.read(offset as u64, size.into()) {
            let wanted_slice = if offset + size < reference.len() {
                &reference[offset..offset + size]
            } else if offset < reference.len() {
                &reference[offset..]
            } else {
                assert_eq!(0, size);
                &reference[0..0]
            };

            eprintln!("offset: {}, size: {}", offset, size);
            assert_eq!(wanted_slice, slice.as_ref());
        } else {
            assert!(reference.len() <= offset);
        }

        true
    }

    quickcheck::quickcheck(prop as fn(Vec<Vec<u8>>, u16, u8) -> bool);
}
