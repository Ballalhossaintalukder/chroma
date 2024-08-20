use super::{single_value_size_tracker::SingleValueSizeTracker, BlockKeyArrowBuilder};
use crate::{
    arrow::types::ArrowWriteableKey,
    key::{CompositeKey, KeyWrapper},
};
use arrow::{
    array::{Array, ArrayRef, StringBuilder},
    datatypes::Field,
    util::bit_util,
};
use parking_lot::RwLock;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone)]
pub struct StringValueStorage {
    inner: Arc<RwLock<Inner>>,
}

struct Inner {
    storage: BTreeMap<CompositeKey, String>,
    size_tracker: SingleValueSizeTracker,
}

impl StringValueStorage {
    pub(in crate::arrow) fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                storage: BTreeMap::new(),
                size_tracker: SingleValueSizeTracker::new(),
            })),
        }
    }

    pub(super) fn get_prefix_size(&self) -> usize {
        let inner = self.inner.read();
        inner.size_tracker.get_prefix_size()
    }

    pub(super) fn get_key_size(&self) -> usize {
        let inner = self.inner.read();
        inner.size_tracker.get_key_size()
    }

    pub(super) fn get_value_size(&self) -> usize {
        let inner = self.inner.read();
        inner.size_tracker.get_value_size()
    }

    pub(super) fn len(&self) -> usize {
        let inner = self.inner.read();
        inner.storage.len()
    }

    pub fn get_min_key(&self) -> Option<CompositeKey> {
        let inner = self.inner.read();
        inner.storage.keys().next().cloned()
    }

    pub(super) fn get_size<K: ArrowWriteableKey>(&self) -> usize {
        let inner = self.inner.read();

        let prefix_size = inner.size_tracker.get_arrow_padded_prefix_size();
        let key_size = inner.size_tracker.get_arrow_padded_key_size();
        let value_size = inner.size_tracker.get_arrow_padded_value_size();

        let prefix_offset_bytes = bit_util::round_upto_multiple_of_64((self.len() + 1) * 4);
        let key_offset_bytes: usize = K::offset_size(self.len());

        let value_offset_bytes = bit_util::round_upto_multiple_of_64((self.len() + 1) * 4);

        prefix_size
            + key_size
            + value_size
            + prefix_offset_bytes
            + key_offset_bytes
            + value_offset_bytes
    }

    pub(super) fn build_keys(&self, builder: BlockKeyArrowBuilder) -> BlockKeyArrowBuilder {
        let inner = self.inner.read();
        let storage = &inner.storage;
        let mut builder = builder;
        for (key, _) in storage.iter() {
            builder.add_key(key.clone());
        }
        builder
    }

    pub fn add(&self, prefix: &str, key: KeyWrapper, value: &str) {
        let mut inner = self.inner.write();

        let key_len = key.get_size();
        inner.storage.insert(
            CompositeKey {
                prefix: prefix.to_string(),
                key,
            },
            value.to_string(),
        );
        inner.size_tracker.add_prefix_size(prefix.len());
        inner.size_tracker.add_key_size(key_len);
        inner.size_tracker.add_value_size(value.len());
    }

    pub fn delete(&self, prefix: &str, key: KeyWrapper) {
        let mut inner = self.inner.write();
        let maybe_removed_prefix_len = prefix.len();
        let maybe_removed_key_len = key.get_size();
        let maybe_removed_value = inner.storage.remove(&CompositeKey {
            prefix: prefix.to_string(),
            key,
        });

        if let Some(value) = maybe_removed_value {
            inner
                .size_tracker
                .subtract_prefix_size(maybe_removed_prefix_len);
            inner.size_tracker.subtract_key_size(maybe_removed_key_len);
            inner.size_tracker.subtract_value_size(value.len());
        }
    }

    pub(super) fn split(&self, split_size: usize) -> (CompositeKey, StringValueStorage) {
        let mut prefix_size = 0;
        let mut key_size = 0;
        let mut value_size = 0;
        let mut split_key = None;

        {
            let inner = self.inner.read();
            let storage = &inner.storage;

            let mut index = 0;
            let mut iter = storage.iter();
            while let Some((key, value)) = iter.next() {
                prefix_size += key.prefix.len();
                key_size += key.key.get_size();
                value_size += value.len();

                // offset sizing
                let prefix_offset_bytes = bit_util::round_upto_multiple_of_64((index + 1) * 4);
                let key_offset_bytes = bit_util::round_upto_multiple_of_64((index + 1) * 4);
                let value_offset_bytes = bit_util::round_upto_multiple_of_64((index + 1) * 4);

                let total_size = bit_util::round_upto_multiple_of_64(prefix_size)
                    + bit_util::round_upto_multiple_of_64(key_size)
                    + bit_util::round_upto_multiple_of_64(value_size)
                    + prefix_offset_bytes
                    + key_offset_bytes
                    + value_offset_bytes;

                if total_size > split_size {
                    split_key = match iter.next() {
                        None => Some(key.clone()),
                        Some((next_key, _)) => Some(next_key.clone()),
                    };
                }
                index += 1;
            }
        }

        let mut inner = self.inner.write();

        match split_key {
            None => panic!("A StringValueStorage should have at least one element to be split."),
            Some(split_key) => {
                let new_delta = inner.storage.split_off(&split_key);
                (
                    split_key,
                    StringValueStorage {
                        inner: Arc::new(RwLock::new(Inner {
                            storage: new_delta,
                            size_tracker: SingleValueSizeTracker::with_values(
                                self.get_prefix_size() - prefix_size,
                                self.get_key_size() - key_size,
                                self.get_value_size() - value_size,
                            ),
                        })),
                    },
                )
            }
        }
    }

    pub(super) fn to_arrow(&self) -> (Field, ArrayRef) {
        let item_capacity = self.len();
        let mut value_builder;
        if item_capacity == 0 {
            value_builder = StringBuilder::new();
        } else {
            value_builder = StringBuilder::with_capacity(item_capacity, self.get_value_size());
        }

        let inner = self.inner.read();
        let storage = &inner.storage;

        for (_, value) in storage.iter() {
            value_builder.append_value(value);
        }

        let value_field = Field::new("value", arrow::datatypes::DataType::Utf8, false);
        let value_arr = value_builder.finish();
        (
            value_field,
            (&value_arr as &dyn Array).slice(0, value_arr.len()),
        )
    }
}
