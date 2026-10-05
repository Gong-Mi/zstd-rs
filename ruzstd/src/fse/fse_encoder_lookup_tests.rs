use super::{
    build_table_from_probabilities, default_ll_table, default_ml_table, default_of_table, FSETable,
    State, SymbolStates,
};
use alloc::vec;
use alloc::vec::Vec;

fn legacy_get(symbol: &SymbolStates, idx: usize, table_size: usize) -> &State {
    let start = idx * symbol.states.len() / table_size;
    symbol.states[start..]
        .iter()
        .find(|state| state.contains(idx))
        .unwrap()
}

fn check_table(table: &FSETable) {
    assert!(table.table_size.is_power_of_two());
    let mut checked = 0;
    for symbol in &table.states {
        if symbol.states.is_empty() {
            continue;
        }
        let bits = symbol.states[0].num_bits;
        let short_count = symbol.states.len() * 2 - (table.table_size >> bits);
        let boundary = short_count << bits;
        for (position, state) in symbol.states.iter().enumerate() {
            let expected_bits = bits + u8::from(position >= short_count);
            let expected_baseline = if position < short_count {
                position << bits
            } else {
                boundary + ((position - short_count) << (bits + 1))
            };
            assert_eq!(state.num_bits, expected_bits);
            assert_eq!(state.baseline, expected_baseline);
            assert_eq!(
                state.last_index,
                expected_baseline + (1 << expected_bits) - 1
            );
        }
        for idx in 0..table.table_size {
            let expected = legacy_get(symbol, idx, table.table_size);
            let actual = symbol.get(idx, table.table_size);
            // Identical record, not just an equal state index or output hash.
            assert!(core::ptr::eq(expected, actual));
            checked += 1;
        }
    }
    assert!(checked > 0);
}

#[test]
fn default_tables_select_identical_records_for_every_state() {
    for table in [default_ll_table(), default_ml_table(), default_of_table()] {
        check_table(&table);
    }
}

#[test]
fn all_two_symbol_frequencies_and_states_match_legacy_lookup() {
    let max_log = if cfg!(miri) { 5 } else { 8 };
    for log in 5..=max_log {
        let size = 1usize << log;
        for frequency in 1..=size {
            let probs = [frequency as i32, (size - frequency) as i32];
            check_table(&build_table_from_probabilities(&probs, log));
        }
    }
}

#[test]
fn large_tables_and_negative_probability_symbols_match_legacy_lookup() {
    let max_log = if cfg!(miri) { 5 } else { 12 };
    for log in 5..=max_log {
        let size = 1usize << log;
        let mut frequencies = vec![1, 2, 3, size / 3, size / 2 - 1, size / 2, size / 2 + 1];
        for power in 1..log {
            let count = 1usize << power;
            frequencies.extend([count - 1, count, count + 1]);
        }
        frequencies.extend([size - 2, size - 1]);
        frequencies.sort_unstable();
        frequencies.dedup();
        for frequency in frequencies {
            if frequency >= size {
                continue;
            }
            let probs = [frequency as i32, (size - frequency - 1) as i32, -1];
            check_table(&build_table_from_probabilities(&probs, log));
        }
    }
}

#[test]
fn sparse_full_alphabet_tables_match_legacy_lookup() {
    let max_log = if cfg!(miri) { 5 } else { 12 };
    let mut seed = 0x7f31_629d_u32;
    for log in 5..=max_log {
        for _ in 0..4 {
            let mut probs = [0i32; 256];
            for _ in 0..(1usize << log) {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                probs[(seed >> 24) as usize] += 1;
            }
            check_table(&build_table_from_probabilities(&probs, log));
        }
    }
}

#[test]
#[should_panic]
fn layout_oracle_rejects_a_corrupted_interval() {
    let mut table = default_ll_table();
    table.states[0].states[0].baseline += 1;
    check_table(&table);
}

#[test]
#[should_panic]
fn empty_symbol_still_panics() {
    let symbol = SymbolStates {
        states: Vec::new(),
        probability: 0,
    };
    symbol.get(0, 32);
}

#[test]
#[should_panic]
fn state_at_table_end_still_panics() {
    let table = default_ll_table();
    table.states[0].get(table.table_size, table.table_size);
}

#[test]
#[should_panic]
fn state_above_table_end_still_panics() {
    let table = default_ll_table();
    table.states[0].get(table.table_size + 1, table.table_size);
}

#[test]
#[should_panic]
fn zero_table_size_still_panics() {
    let table = default_ll_table();
    table.states[0].get(0, 0);
}

#[test]
#[should_panic]
fn maximal_state_index_still_panics() {
    let table = default_ll_table();
    table.states[0].get(usize::MAX, table.table_size);
}
