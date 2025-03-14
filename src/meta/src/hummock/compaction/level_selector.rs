//  Copyright 2023 RisingWave Labs
//
//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at
//
//  http://www.apache.org/licenses/LICENSE-2.0
//
//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.
//
// Copyright (c) 2011-present, Facebook, Inc.  All rights reserved.
// This source code is licensed under both the GPLv2 (found in the
// COPYING file in the root directory) and Apache 2.0 License
// (found in the LICENSE.Apache file in the root directory).
use std::sync::Arc;

use risingwave_hummock_sdk::HummockCompactionTaskId;
use risingwave_pb::hummock::hummock_version::Levels;
use risingwave_pb::hummock::{compact_task, CompactionConfig};

use super::picker::{SpaceReclaimCompactionPicker, TtlReclaimCompactionPicker};
use super::{
    create_compaction_task, LevelCompactionPicker, ManualCompactionOption, ManualCompactionPicker,
    TierCompactionPicker,
};
use crate::hummock::compaction::compaction_config::CompactionConfigBuilder;
use crate::hummock::compaction::overlap_strategy::OverlapStrategy;
use crate::hummock::compaction::{
    create_overlap_strategy, CompactionPicker, CompactionTask, LocalPickerStatistic,
    LocalSelectorStatistic, MinOverlappingPicker,
};
use crate::hummock::level_handler::LevelHandler;
use crate::rpc::metrics::MetaMetrics;

const SCORE_BASE: u64 = 100;

pub mod selector_option {
    use std::collections::HashSet;
    use std::sync::Arc;

    use risingwave_pb::hummock::CompactionConfig;

    use crate::hummock::compaction::ManualCompactionOption;

    #[derive(Clone)]
    pub struct DynamicLevelSelectorOption {
        pub compaction_config: Arc<CompactionConfig>,
    }

    #[derive(Clone)]
    pub struct ManualCompactionSelectorOption {
        pub compaction_config: Arc<CompactionConfig>,
        pub option: ManualCompactionOption,
    }

    #[derive(Clone)]
    pub struct SpaceReclaimCompactionSelectorOption {
        pub compaction_config: Arc<CompactionConfig>,
        pub all_table_ids: HashSet<u32>,
    }

    #[derive(Clone)]
    pub struct TtlCompactionSelectorOption {
        pub compaction_config: Arc<CompactionConfig>,
        // todo: check table_option
    }
}

pub enum SelectorOption {
    Dynamic(selector_option::DynamicLevelSelectorOption),
    Manual(selector_option::ManualCompactionSelectorOption),
    SpaceReclaim(selector_option::SpaceReclaimCompactionSelectorOption),
    Ttl(selector_option::TtlCompactionSelectorOption),
}

impl SelectorOption {
    pub fn as_dynamic(&self) -> Option<selector_option::DynamicLevelSelectorOption> {
        match self {
            Self::Dynamic(o) => Some(o.clone()),
            _ => None,
        }
    }

    pub fn as_manual(&self) -> Option<selector_option::ManualCompactionSelectorOption> {
        match self {
            Self::Manual(o) => Some(o.clone()),
            _ => None,
        }
    }

    pub fn as_space_reclaim(
        &self,
    ) -> Option<selector_option::SpaceReclaimCompactionSelectorOption> {
        match self {
            Self::SpaceReclaim(o) => Some(o.clone()),
            _ => None,
        }
    }

    pub fn as_ttl(&self) -> Option<selector_option::TtlCompactionSelectorOption> {
        match self {
            Self::Ttl(o) => Some(o.clone()),
            _ => None,
        }
    }
}

pub trait LevelSelector: Sync + Send {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        levels: &Levels,
        level_handlers: &mut [LevelHandler],
        selector_stats: &mut LocalSelectorStatistic,
    ) -> Option<CompactionTask>;

    fn report_statistic_metrics(&self, _metrics: &MetaMetrics) {}

    fn name(&self) -> &'static str;

    fn task_type(&self) -> compact_task::TaskType;

    fn try_update(&mut self, selector_option: SelectorOption);
}

#[derive(Default)]
pub struct SelectContext {
    pub level_max_bytes: Vec<u64>,

    // All data will be placed in the last level. When the cluster is empty, the files in L0 will
    // be compact to `max_level`, and the `max_level` would be `base_level`. When the total
    // size of the files in  `base_level` reaches its capacity, we will place data in a higher
    // level, which equals to `base_level -= 1;`.
    pub base_level: usize,
    pub score_levels: Vec<(u64, usize, usize)>,
}

pub struct DynamicLevelSelectorCore {
    config: Arc<CompactionConfig>,
}

pub struct DynamicLevelSelector {
    dynamic_level_core: DynamicLevelSelectorCore,
    overlap_strategy: Arc<dyn OverlapStrategy>,
}

impl Default for DynamicLevelSelector {
    fn default() -> Self {
        let config = Arc::new(CompactionConfigBuilder::new().build());
        let overlap_strategy = create_overlap_strategy(config.compaction_mode());
        DynamicLevelSelector::new(config, overlap_strategy)
    }
}

impl DynamicLevelSelector {
    pub fn new(config: Arc<CompactionConfig>, overlap_strategy: Arc<dyn OverlapStrategy>) -> Self {
        Self {
            dynamic_level_core: DynamicLevelSelectorCore::new(config),
            overlap_strategy,
        }
    }

    fn update_impl(&mut self, selector_option: selector_option::DynamicLevelSelectorOption) {
        self.dynamic_level_core =
            DynamicLevelSelectorCore::new(selector_option.compaction_config.clone());
        self.overlap_strategy =
            create_overlap_strategy(selector_option.compaction_config.compaction_mode());
    }
}

impl DynamicLevelSelectorCore {
    pub fn new(config: Arc<CompactionConfig>) -> Self {
        Self { config }
    }

    pub fn get_config(&self) -> &CompactionConfig {
        self.config.as_ref()
    }

    fn create_compaction_picker(
        &self,
        select_level: usize,
        target_level: usize,
        overlap_strategy: Arc<dyn OverlapStrategy>,
    ) -> Box<dyn CompactionPicker> {
        if select_level == 0 {
            if target_level == 0 {
                Box::new(TierCompactionPicker::new(
                    self.config.clone(),
                    overlap_strategy,
                ))
            } else {
                Box::new(LevelCompactionPicker::new(
                    target_level,
                    self.config.clone(),
                    overlap_strategy,
                ))
            }
        } else {
            assert_eq!(select_level + 1, target_level);
            Box::new(MinOverlappingPicker::new(
                select_level,
                target_level,
                self.config.max_bytes_for_level_base,
                overlap_strategy,
            ))
        }
    }

    // TODO: calculate this scores in apply compact result.
    /// `calculate_level_base_size` calculate base level and the base size of LSM tree build for
    /// current dataset. In other words,  `level_max_bytes` is our compaction goal which shall
    /// reach. This algorithm refers to the implementation in  `</>https://github.com/facebook/rocksdb/blob/v7.2.2/db/version_set.cc#L3706</>`
    pub fn calculate_level_base_size(&self, levels: &Levels) -> SelectContext {
        let mut first_non_empty_level = 0;
        let mut max_level_size = 0;
        let mut ctx = SelectContext::default();

        for level in &levels.levels {
            if level.total_file_size > 0 && first_non_empty_level == 0 {
                first_non_empty_level = level.level_idx as usize;
            }
            max_level_size = std::cmp::max(max_level_size, level.total_file_size);
        }

        ctx.level_max_bytes
            .resize(self.config.max_level as usize + 1, u64::MAX);

        if max_level_size == 0 {
            // Use the bottommost level.
            ctx.base_level = self.config.max_level as usize;
            return ctx;
        }

        let base_bytes_max = self.config.max_bytes_for_level_base;
        let base_bytes_min = base_bytes_max / self.config.max_bytes_for_level_multiplier;

        let mut cur_level_size = max_level_size;
        for _ in first_non_empty_level..self.config.max_level as usize {
            cur_level_size /= self.config.max_bytes_for_level_multiplier;
        }

        let base_level_size = if cur_level_size <= base_bytes_min {
            // Case 1. If we make target size of last level to be max_level_size,
            // target size of the first non-empty level would be smaller than
            // base_bytes_min. We set it be base_bytes_min.
            ctx.base_level = first_non_empty_level;
            base_bytes_min + 1
        } else {
            ctx.base_level = first_non_empty_level;
            while ctx.base_level > 1 && cur_level_size > base_bytes_max {
                ctx.base_level -= 1;
                cur_level_size /= self.config.max_bytes_for_level_multiplier;
            }
            std::cmp::min(base_bytes_max, cur_level_size)
        };

        let level_multiplier = self.config.max_bytes_for_level_multiplier as f64;
        let mut level_size = base_level_size;
        for i in ctx.base_level..=self.config.max_level as usize {
            // Don't set any level below base_bytes_max. Otherwise, the LSM can
            // assume an hourglass shape where L1+ sizes are smaller than L0. This
            // causes compaction scoring, which depends on level sizes, to favor L1+
            // at the expense of L0, which may fill up and stall.
            ctx.level_max_bytes[i] = std::cmp::max(level_size, base_bytes_max);
            level_size = (level_size as f64 * level_multiplier) as u64;
        }
        ctx
    }

    fn get_priority_levels(&self, levels: &Levels, handlers: &[LevelHandler]) -> SelectContext {
        let mut ctx = self.calculate_level_base_size(levels);

        let idle_file_count = levels
            .l0
            .as_ref()
            .unwrap()
            .sub_levels
            .iter()
            .map(|level| level.table_infos.len())
            .sum::<usize>()
            - handlers[0].get_pending_file_count();
        let max_l0_score = std::cmp::max(
            SCORE_BASE * 2,
            levels.l0.as_ref().unwrap().sub_levels.len() as u64 * SCORE_BASE
                / self.config.level0_tier_compact_file_number,
        );

        let total_size = levels.l0.as_ref().unwrap().total_file_size
            - handlers[0].get_pending_output_file_size(ctx.base_level as u32);
        if idle_file_count > 0 {
            // trigger intra-l0 compaction at first when the number of files is too large.
            let l0_score =
                idle_file_count as u64 * SCORE_BASE / self.config.level0_tier_compact_file_number;
            ctx.score_levels
                .push((std::cmp::min(l0_score, max_l0_score), 0, 0));
            let score = total_size * SCORE_BASE / self.config.max_bytes_for_level_base;
            ctx.score_levels.push((score, 0, ctx.base_level));
        }

        // The bottommost level can not be input level.
        for level in &levels.levels {
            let level_idx = level.level_idx as usize;
            if level_idx < ctx.base_level || level_idx >= self.config.max_level as usize {
                continue;
            }
            let upper_level = if level_idx == ctx.base_level {
                0
            } else {
                level_idx - 1
            };
            let total_size = level.total_file_size
                + handlers[upper_level].get_pending_output_file_size(level.level_idx)
                - handlers[level_idx].get_pending_output_file_size(level.level_idx + 1);
            if total_size == 0 {
                continue;
            }
            ctx.score_levels.push((
                total_size * SCORE_BASE / ctx.level_max_bytes[level_idx],
                level_idx,
                level_idx + 1,
            ));
        }

        // sort reverse to pick the largest one.
        ctx.score_levels.sort_by(|a, b| b.0.cmp(&a.0));
        ctx
    }
}

impl LevelSelector for DynamicLevelSelector {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        levels: &Levels,
        level_handlers: &mut [LevelHandler],
        selector_stats: &mut LocalSelectorStatistic,
    ) -> Option<CompactionTask> {
        let ctx = self
            .dynamic_level_core
            .get_priority_levels(levels, level_handlers);
        for (score, select_level, target_level) in ctx.score_levels {
            if score <= SCORE_BASE {
                return None;
            }
            let mut picker = self.dynamic_level_core.create_compaction_picker(
                select_level,
                target_level,
                self.overlap_strategy.clone(),
            );
            let mut stats = LocalPickerStatistic::default();
            if let Some(ret) = picker.pick_compaction(levels, level_handlers, &mut stats) {
                ret.add_pending_task(task_id, level_handlers);
                return Some(create_compaction_task(
                    self.dynamic_level_core.get_config(),
                    ret,
                    ctx.base_level,
                    self.task_type(),
                ));
            }
            selector_stats
                .skip_picker
                .push((select_level, target_level, stats));
        }
        None
    }

    fn name(&self) -> &'static str {
        "DynamicLevelSelector"
    }

    fn task_type(&self) -> compact_task::TaskType {
        compact_task::TaskType::Dynamic
    }

    fn try_update(&mut self, selector_option: SelectorOption) {
        let selector_option = selector_option
            .as_dynamic()
            .expect("try_update to as_manual");

        if *self.dynamic_level_core.get_config() != *selector_option.compaction_config {
            self.update_impl(selector_option)
        }
    }
}

pub struct ManualCompactionSelector {
    dynamic_level_core: DynamicLevelSelectorCore,
    option: ManualCompactionOption,
    overlap_strategy: Arc<dyn OverlapStrategy>,
}

impl ManualCompactionSelector {
    pub fn new(
        config: Arc<CompactionConfig>,
        overlap_strategy: Arc<dyn OverlapStrategy>,
        option: ManualCompactionOption,
    ) -> Self {
        Self {
            dynamic_level_core: DynamicLevelSelectorCore::new(config),
            option,
            overlap_strategy,
        }
    }

    fn update_impl(&mut self, selector_option: selector_option::ManualCompactionSelectorOption) {
        self.dynamic_level_core =
            DynamicLevelSelectorCore::new(selector_option.compaction_config.clone());
        self.option = selector_option.option;
        self.overlap_strategy =
            create_overlap_strategy(selector_option.compaction_config.compaction_mode())
    }
}

impl LevelSelector for ManualCompactionSelector {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        levels: &Levels,
        level_handlers: &mut [LevelHandler],
        _selector_stats: &mut LocalSelectorStatistic,
    ) -> Option<CompactionTask> {
        let ctx = self.dynamic_level_core.calculate_level_base_size(levels);
        let (mut picker, base_level) = {
            let target_level = if self.option.level == 0 {
                ctx.base_level
            } else if self.option.level == self.dynamic_level_core.get_config().max_level as usize {
                self.option.level
            } else {
                self.option.level + 1
            };
            if self.option.level > 0 && self.option.level < ctx.base_level {
                return None;
            }
            (
                ManualCompactionPicker::new(
                    self.overlap_strategy.clone(),
                    self.option.clone(),
                    target_level,
                ),
                ctx.base_level,
            )
        };

        let compaction_input =
            picker.pick_compaction(levels, level_handlers, &mut LocalPickerStatistic::default())?;
        compaction_input.add_pending_task(task_id, level_handlers);

        Some(create_compaction_task(
            self.dynamic_level_core.get_config(),
            compaction_input,
            base_level,
            self.task_type(),
        ))
    }

    fn name(&self) -> &'static str {
        "ManualCompactionSelector"
    }

    fn task_type(&self) -> compact_task::TaskType {
        compact_task::TaskType::Manual
    }

    fn try_update(&mut self, selector_option: SelectorOption) {
        let selector_option = selector_option
            .as_manual()
            .expect("try_update to as_manual");

        if *self.dynamic_level_core.get_config() != *selector_option.compaction_config
            || self.option != selector_option.option
        {
            self.update_impl(selector_option)
        }
    }
}

pub struct SpaceReclaimCompactionSelector {
    dynamic_level_core: DynamicLevelSelectorCore,
    picker: SpaceReclaimCompactionPicker,
}

impl SpaceReclaimCompactionSelector {
    pub fn new(selector_option: selector_option::SpaceReclaimCompactionSelectorOption) -> Self {
        Self {
            picker: SpaceReclaimCompactionPicker::new(
                selector_option.compaction_config.max_space_reclaim_bytes,
                selector_option.all_table_ids,
            ),
            dynamic_level_core: DynamicLevelSelectorCore::new(selector_option.compaction_config),
        }
    }

    fn update_impl(
        &mut self,
        selector_option: selector_option::SpaceReclaimCompactionSelectorOption,
    ) {
        self.dynamic_level_core =
            DynamicLevelSelectorCore::new(selector_option.compaction_config.clone());

        self.picker = SpaceReclaimCompactionPicker::new(
            selector_option.compaction_config.max_space_reclaim_bytes,
            selector_option.all_table_ids,
        );
    }
}

impl LevelSelector for SpaceReclaimCompactionSelector {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        levels: &Levels,
        level_handlers: &mut [LevelHandler],
        _selector_stats: &mut LocalSelectorStatistic,
    ) -> Option<CompactionTask> {
        let ctx = self.dynamic_level_core.calculate_level_base_size(levels);
        let compaction_input = self.picker.pick_compaction(
            levels,
            level_handlers,
            &mut LocalPickerStatistic::default(),
        )?;
        compaction_input.add_pending_task(task_id, level_handlers);

        Some(create_compaction_task(
            self.dynamic_level_core.get_config(),
            compaction_input,
            ctx.base_level,
            self.task_type(),
        ))
    }

    fn name(&self) -> &'static str {
        "SpaceReclaimCompaction"
    }

    fn task_type(&self) -> compact_task::TaskType {
        compact_task::TaskType::SpaceReclaim
    }

    fn try_update(&mut self, selector_option: SelectorOption) {
        let selector_option = selector_option
            .as_space_reclaim()
            .expect("try_update to as_space_reclaim");

        if (*self.dynamic_level_core.get_config() != *selector_option.compaction_config)
            || self.picker.all_table_ids != selector_option.all_table_ids
        {
            self.update_impl(selector_option)
        }
    }
}

pub struct TtlCompactionSelector {
    dynamic_level_core: DynamicLevelSelectorCore,
    picker: TtlReclaimCompactionPicker,
}

impl TtlCompactionSelector {
    pub fn new(config: Arc<CompactionConfig>) -> Self {
        Self {
            picker: TtlReclaimCompactionPicker::new(config.max_space_reclaim_bytes),
            dynamic_level_core: DynamicLevelSelectorCore::new(config),
        }
    }

    fn update_impl(&mut self, selector_option: selector_option::TtlCompactionSelectorOption) {
        self.dynamic_level_core =
            DynamicLevelSelectorCore::new(selector_option.compaction_config.clone());
        self.picker = TtlReclaimCompactionPicker::new(
            selector_option.compaction_config.max_space_reclaim_bytes,
        );
    }
}

impl LevelSelector for TtlCompactionSelector {
    fn pick_compaction(
        &mut self,
        task_id: HummockCompactionTaskId,
        levels: &Levels,
        level_handlers: &mut [LevelHandler],
        _selector_stats: &mut LocalSelectorStatistic,
    ) -> Option<CompactionTask> {
        let ctx = self.dynamic_level_core.calculate_level_base_size(levels);
        let compaction_input = self.picker.pick_compaction(
            levels,
            level_handlers,
            &mut LocalPickerStatistic::default(),
        )?;
        compaction_input.add_pending_task(task_id, level_handlers);

        Some(create_compaction_task(
            self.dynamic_level_core.get_config(),
            compaction_input,
            ctx.base_level,
            self.task_type(),
        ))
    }

    fn name(&self) -> &'static str {
        "TtlCompaction"
    }

    fn task_type(&self) -> compact_task::TaskType {
        compact_task::TaskType::Ttl
    }

    fn try_update(&mut self, selector_option: SelectorOption) {
        let selector_option = selector_option.as_ttl().expect("try_update to as_manual");

        if *self.dynamic_level_core.get_config() != *selector_option.compaction_config {
            self.update_impl(selector_option)
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::ops::Range;

    use itertools::Itertools;
    use risingwave_common::constants::hummock::CompactionFilterFlag;
    use risingwave_pb::hummock::compaction_config::CompactionMode;
    use risingwave_pb::hummock::{KeyRange, Level, LevelType, OverlappingLevel, SstableInfo};

    use super::*;
    use crate::hummock::compaction::compaction_config::CompactionConfigBuilder;
    use crate::hummock::compaction::overlap_strategy::RangeOverlapStrategy;
    use crate::hummock::test_utils::iterator_test_key_of_epoch;

    pub fn push_table_level0_overlapping(levels: &mut Levels, sst: SstableInfo) {
        levels.l0.as_mut().unwrap().total_file_size += sst.file_size;
        levels.l0.as_mut().unwrap().sub_levels.push(Level {
            level_idx: 0,
            level_type: LevelType::Overlapping as i32,
            total_file_size: sst.file_size,
            sub_level_id: sst.id,
            table_infos: vec![sst],
        });
    }

    pub fn push_table_level0_nonoverlapping(levels: &mut Levels, sst: SstableInfo) {
        push_table_level0_overlapping(levels, sst);
        levels
            .l0
            .as_mut()
            .unwrap()
            .sub_levels
            .last_mut()
            .unwrap()
            .level_type = LevelType::Nonoverlapping as i32;
    }

    pub fn push_tables_level0_nonoverlapping(levels: &mut Levels, table_infos: Vec<SstableInfo>) {
        let total_file_size = table_infos.iter().map(|table| table.file_size).sum::<u64>();
        let sub_level_id = table_infos[0].id;
        levels.l0.as_mut().unwrap().total_file_size += total_file_size;
        levels.l0.as_mut().unwrap().sub_levels.push(Level {
            level_idx: 0,
            level_type: LevelType::Nonoverlapping as i32,
            total_file_size,
            sub_level_id,
            table_infos,
        });
    }

    pub fn generate_table(
        id: u64,
        table_prefix: u64,
        left: usize,
        right: usize,
        epoch: u64,
    ) -> SstableInfo {
        SstableInfo {
            id,
            key_range: Some(KeyRange {
                left: iterator_test_key_of_epoch(table_prefix, left, epoch),
                right: iterator_test_key_of_epoch(table_prefix, right, epoch),
                right_exclusive: false,
            }),
            file_size: (right - left + 1) as u64,
            table_ids: vec![],
            meta_offset: 0,
            stale_key_count: 0,
            total_key_count: 0,
            divide_version: 0,
        }
    }

    pub fn generate_table_with_table_ids(
        id: u64,
        table_prefix: u64,
        left: usize,
        right: usize,
        epoch: u64,
        table_ids: Vec<u32>,
    ) -> SstableInfo {
        SstableInfo {
            id,
            key_range: Some(KeyRange {
                left: iterator_test_key_of_epoch(table_prefix, left, epoch),
                right: iterator_test_key_of_epoch(table_prefix, right, epoch),
                right_exclusive: false,
            }),
            file_size: (right - left + 1) as u64,
            table_ids,
            meta_offset: 0,
            stale_key_count: 0,
            total_key_count: 0,
            divide_version: 0,
        }
    }

    pub fn generate_tables(
        ids: Range<u64>,
        keys: Range<usize>,
        epoch: u64,
        file_size: u64,
    ) -> Vec<SstableInfo> {
        let step = (keys.end - keys.start) / (ids.end - ids.start) as usize;
        let mut start = keys.start;
        let mut tables = vec![];
        for id in ids {
            let mut table = generate_table(id, 1, start, start + step - 1, epoch);
            table.file_size = file_size;
            tables.push(table);
            start += step;
        }
        tables
    }

    pub fn generate_level(level_idx: u32, table_infos: Vec<SstableInfo>) -> Level {
        let total_file_size = table_infos.iter().map(|sst| sst.file_size).sum();
        Level {
            level_idx,
            level_type: LevelType::Nonoverlapping as i32,
            table_infos,
            total_file_size,
            sub_level_id: 0,
        }
    }

    /// Returns a `OverlappingLevel`, with each `table_infos`'s element placed in a nonoverlapping
    /// sub-level.
    pub fn generate_l0_nonoverlapping_sublevels(table_infos: Vec<SstableInfo>) -> OverlappingLevel {
        let total_file_size = table_infos.iter().map(|table| table.file_size).sum::<u64>();
        OverlappingLevel {
            sub_levels: table_infos
                .into_iter()
                .enumerate()
                .map(|(idx, table)| Level {
                    level_idx: 0,
                    level_type: LevelType::Nonoverlapping as i32,
                    total_file_size: table.file_size,
                    sub_level_id: idx as u64,
                    table_infos: vec![table],
                })
                .collect_vec(),
            total_file_size,
        }
    }

    /// Returns a `OverlappingLevel`, with each `table_infos`'s element placed in a overlapping
    /// sub-level.
    pub fn generate_l0_overlapping_sublevels(
        table_infos: Vec<Vec<SstableInfo>>,
    ) -> OverlappingLevel {
        let mut l0 = OverlappingLevel {
            sub_levels: table_infos
                .into_iter()
                .enumerate()
                .map(|(idx, table)| Level {
                    level_idx: 0,
                    level_type: LevelType::Overlapping as i32,
                    total_file_size: table.iter().map(|table| table.file_size).sum::<u64>(),
                    sub_level_id: idx as u64,
                    table_infos: table.clone(),
                })
                .collect_vec(),
            total_file_size: 0,
        };
        l0.total_file_size = l0.sub_levels.iter().map(|l| l.total_file_size).sum::<u64>();
        l0
    }

    pub(crate) fn assert_compaction_task(
        compact_task: &CompactionTask,
        level_handlers: &[LevelHandler],
    ) {
        for i in &compact_task.input.input_levels {
            for t in &i.table_infos {
                assert!(level_handlers[i.level_idx as usize].is_pending_compact(&t.id));
            }
        }
    }

    #[test]
    fn test_dynamic_level() {
        let config = CompactionConfigBuilder::new()
            .max_bytes_for_level_base(100)
            .max_level(4)
            .max_bytes_for_level_multiplier(5)
            .max_compaction_bytes(1)
            .level0_tier_compact_file_number(2)
            .compaction_mode(CompactionMode::Range as i32)
            .build();
        let selector = DynamicLevelSelectorCore::new(Arc::new(config));
        let levels = vec![
            generate_level(1, vec![]),
            generate_level(2, generate_tables(0..5, 0..1000, 3, 10)),
            generate_level(3, generate_tables(5..10, 0..1000, 2, 50)),
            generate_level(4, generate_tables(10..15, 0..1000, 1, 200)),
        ];
        let mut levels = Levels {
            levels,
            l0: Some(generate_l0_nonoverlapping_sublevels(vec![])),
        };
        let ctx = selector.calculate_level_base_size(&levels);
        assert_eq!(ctx.base_level, 2);
        assert_eq!(ctx.level_max_bytes[2], 100);
        assert_eq!(ctx.level_max_bytes[3], 200);
        assert_eq!(ctx.level_max_bytes[4], 1000);

        levels.levels[3]
            .table_infos
            .append(&mut generate_tables(15..20, 2000..3000, 1, 400));
        levels.levels[3].total_file_size = levels.levels[3]
            .table_infos
            .iter()
            .map(|sst| sst.file_size)
            .sum::<u64>();

        let ctx = selector.calculate_level_base_size(&levels);
        // data size increase, so we need increase one level to place more data.
        assert_eq!(ctx.base_level, 1);
        assert_eq!(ctx.level_max_bytes[1], 100);
        assert_eq!(ctx.level_max_bytes[2], 120);
        assert_eq!(ctx.level_max_bytes[3], 600);
        assert_eq!(ctx.level_max_bytes[4], 3000);

        // append a large data to L0 but it does not change the base size of LSM tree.
        push_tables_level0_nonoverlapping(&mut levels, generate_tables(20..26, 0..1000, 1, 100));

        let ctx = selector.calculate_level_base_size(&levels);
        assert_eq!(ctx.base_level, 1);
        assert_eq!(ctx.level_max_bytes[1], 100);
        assert_eq!(ctx.level_max_bytes[2], 120);
        assert_eq!(ctx.level_max_bytes[3], 600);
        assert_eq!(ctx.level_max_bytes[4], 3000);

        levels.l0.as_mut().unwrap().sub_levels.clear();
        levels.l0.as_mut().unwrap().total_file_size = 0;
        levels.levels[0].table_infos = generate_tables(26..32, 0..1000, 1, 100);
        levels.levels[0].total_file_size = levels.levels[0]
            .table_infos
            .iter()
            .map(|sst| sst.file_size)
            .sum::<u64>();

        let ctx = selector.calculate_level_base_size(&levels);
        assert_eq!(ctx.base_level, 1);
        assert_eq!(ctx.level_max_bytes[1], 100);
        assert_eq!(ctx.level_max_bytes[2], 120);
        assert_eq!(ctx.level_max_bytes[3], 600);
        assert_eq!(ctx.level_max_bytes[4], 3000);
    }

    #[test]
    fn test_pick_compaction() {
        let config = CompactionConfigBuilder::new()
            .max_bytes_for_level_base(200)
            .max_level(4)
            .max_bytes_for_level_multiplier(5)
            .max_compaction_bytes(10000)
            .level0_tier_compact_file_number(4)
            .compaction_mode(CompactionMode::Range as i32)
            .build();
        let levels = vec![
            generate_level(1, vec![]),
            generate_level(2, generate_tables(0..5, 0..1000, 3, 10)),
            generate_level(3, generate_tables(5..10, 0..1000, 2, 50)),
            generate_level(4, generate_tables(10..15, 0..1000, 1, 200)),
        ];
        let mut levels = Levels {
            levels,
            l0: Some(generate_l0_nonoverlapping_sublevels(generate_tables(
                15..25,
                0..600,
                3,
                10,
            ))),
        };

        let mut selector = DynamicLevelSelector::new(
            Arc::new(config.clone()),
            Arc::new(RangeOverlapStrategy::default()),
        );
        let mut levels_handlers = (0..5).map(LevelHandler::new).collect_vec();
        let mut local_stats = LocalSelectorStatistic::default();
        let compaction = selector
            .pick_compaction(1, &levels, &mut levels_handlers, &mut local_stats)
            .unwrap();
        // trivial move.
        assert_compaction_task(&compaction, &levels_handlers);
        assert_eq!(compaction.input.input_levels[0].level_idx, 0);
        assert!(compaction.input.input_levels[1].table_infos.is_empty());
        assert_eq!(compaction.input.target_level, 0);

        let compaction_filter_flag = CompactionFilterFlag::STATE_CLEAN | CompactionFilterFlag::TTL;
        let config = CompactionConfigBuilder::with_config(config)
            .max_bytes_for_level_base(100)
            .compaction_filter_mask(compaction_filter_flag.into())
            .build();
        let mut selector = DynamicLevelSelector::new(
            Arc::new(config.clone()),
            Arc::new(RangeOverlapStrategy::default()),
        );

        levels.l0.as_mut().unwrap().sub_levels.clear();
        levels.l0.as_mut().unwrap().total_file_size = 0;
        push_tables_level0_nonoverlapping(&mut levels, generate_tables(15..25, 0..600, 3, 20));
        let mut levels_handlers = (0..5).map(LevelHandler::new).collect_vec();
        let compaction = selector
            .pick_compaction(1, &levels, &mut levels_handlers, &mut local_stats)
            .unwrap();
        assert_compaction_task(&compaction, &levels_handlers);
        assert_eq!(compaction.input.input_levels[0].level_idx, 0);
        assert_eq!(compaction.input.target_level, 2);
        assert_eq!(compaction.target_file_size, config.target_file_size_base);

        levels_handlers[0].remove_task(1);
        levels_handlers[2].remove_task(1);
        levels.l0.as_mut().unwrap().sub_levels.clear();
        levels.levels[1].table_infos = generate_tables(20..30, 0..1000, 3, 10);
        let compaction = selector
            .pick_compaction(2, &levels, &mut levels_handlers, &mut local_stats)
            .unwrap();
        assert_compaction_task(&compaction, &levels_handlers);
        assert_eq!(compaction.input.input_levels[0].level_idx, 3);
        assert_eq!(compaction.input.target_level, 4);
        assert_eq!(compaction.input.input_levels[0].table_infos.len(), 1);
        assert_eq!(compaction.input.input_levels[1].table_infos.len(), 1);
        assert_eq!(
            compaction.target_file_size,
            config.target_file_size_base * 2
        );
        assert_eq!(compaction.compression_algorithm.as_str(), "Lz4",);
        // no compaction need to be scheduled because we do not calculate the size of pending files
        // to score.
        let compaction =
            selector.pick_compaction(2, &levels, &mut levels_handlers, &mut local_stats);
        assert!(compaction.is_none());
    }
}
