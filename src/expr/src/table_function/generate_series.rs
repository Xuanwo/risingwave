// Copyright 2023 RisingWave Labs
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

use std::sync::Arc;

use anyhow::anyhow;
use itertools::multizip;
use num_traits::Zero;
use risingwave_common::array::{
    Array, ArrayBuilder, ArrayImpl, ArrayRef, DataChunk, I32Array, IntervalArray,
    NaiveDateTimeArray,
};
use risingwave_common::types::{CheckedAdd, IsNegative, Scalar, ScalarRef};
use risingwave_common::util::iter_util::ZipEqDebug;

use super::*;
use crate::ExprError;

#[derive(Debug)]
pub struct GenerateSeries<T: Array, S: Array, const STOP_INCLUSIVE: bool> {
    start: BoxedExpression,
    stop: BoxedExpression,
    step: BoxedExpression,
    chunk_size: usize,
    _phantom: std::marker::PhantomData<(T, S)>,
}

impl<T: Array, S: Array, const STOP_INCLUSIVE: bool> GenerateSeries<T, S, STOP_INCLUSIVE>
where
    T::OwnedItem: for<'a> PartialOrd<T::RefItem<'a>>,
    T::OwnedItem: for<'a> CheckedAdd<S::RefItem<'a>, Output = T::OwnedItem>,
    for<'a> S::RefItem<'a>: IsNegative,
{
    fn new(
        start: BoxedExpression,
        stop: BoxedExpression,
        step: BoxedExpression,
        chunk_size: usize,
    ) -> Self {
        Self {
            start,
            stop,
            step,
            chunk_size,
            _phantom: Default::default(),
        }
    }

    fn eval_row(
        &self,
        start: T::RefItem<'_>,
        stop: T::RefItem<'_>,
        step: S::RefItem<'_>,
    ) -> Result<ArrayRef> {
        if step.is_zero() {
            return Err(ExprError::InvalidParam {
                name: "step",
                reason: "must be non-zero".to_string(),
            });
        }

        let mut builder = T::Builder::new(self.chunk_size);

        let mut cur: T::OwnedItem = start.to_owned_scalar();

        while if step.is_negative() {
            if STOP_INCLUSIVE {
                cur >= stop
            } else {
                cur > stop
            }
        } else if STOP_INCLUSIVE {
            cur <= stop
        } else {
            cur < stop
        } {
            builder.append(Some(cur.as_scalar_ref()));
            cur = cur.checked_add(step).ok_or(ExprError::NumericOutOfRange)?;
        }

        Ok(Arc::new(builder.finish().into()))
    }
}

impl<T: Array, S: Array, const STOP_INCLUSIVE: bool> TableFunction
    for GenerateSeries<T, S, STOP_INCLUSIVE>
where
    T::OwnedItem: for<'a> PartialOrd<T::RefItem<'a>>,
    T::OwnedItem: for<'a> CheckedAdd<S::RefItem<'a>, Output = T::OwnedItem>,
    for<'a> S::RefItem<'a>: IsNegative,
    for<'a> &'a T: From<&'a ArrayImpl>,
    for<'a> &'a S: From<&'a ArrayImpl>,
{
    fn return_type(&self) -> DataType {
        self.start.return_type()
    }

    fn eval(&self, input: &DataChunk) -> Result<Vec<ArrayRef>> {
        let ret_start = self.start.eval_checked(input)?;
        let arr_start: &T = ret_start.as_ref().into();
        let ret_stop = self.stop.eval_checked(input)?;
        let arr_stop: &T = ret_stop.as_ref().into();

        let ret_step = self.step.eval_checked(input)?;
        let arr_step: &S = ret_step.as_ref().into();

        let bitmap = input.visibility();
        let mut output_arrays: Vec<ArrayRef> = vec![];

        match bitmap {
            Some(bitmap) => {
                for ((start, stop, step), visible) in
                    multizip((arr_start.iter(), arr_stop.iter(), arr_step.iter()))
                        .zip_eq_debug(bitmap.iter())
                {
                    let array = if !visible {
                        empty_array(self.return_type())
                    } else if let (Some(start), Some(stop), Some(step)) = (start, stop, step) {
                        self.eval_row(start, stop, step)?
                    } else {
                        empty_array(self.return_type())
                    };
                    output_arrays.push(array);
                }
            }
            None => {
                for (start, stop, step) in
                    multizip((arr_start.iter(), arr_stop.iter(), arr_step.iter()))
                {
                    let array = if let (Some(start), Some(stop), Some(step)) = (start, stop, step) {
                        self.eval_row(start, stop, step)?
                    } else {
                        empty_array(self.return_type())
                    };
                    output_arrays.push(array);
                }
            }
        }

        Ok(output_arrays)
    }
}

pub fn new_generate_series<const STOP_INCLUSIVE: bool>(
    prost: &TableFunctionProst,
    chunk_size: usize,
) -> Result<BoxedTableFunction> {
    let return_type = DataType::from(prost.get_return_type().unwrap());
    let args: Vec<_> = prost.args.iter().map(expr_build_from_prost).try_collect()?;
    let [start, stop, step]: [_; 3] = args.try_into().unwrap();

    match return_type {
        DataType::Timestamp => Ok(GenerateSeries::<
            NaiveDateTimeArray,
            IntervalArray,
            STOP_INCLUSIVE,
        >::new(start, stop, step, chunk_size)
        .boxed()),
        DataType::Int32 => Ok(GenerateSeries::<I32Array, I32Array, STOP_INCLUSIVE>::new(
            start, stop, step, chunk_size,
        )
        .boxed()),
        _ => Err(ExprError::Internal(anyhow!(
            "the return type of Generate Series Function is incorrect".to_string(),
        ))),
    }
}

#[cfg(test)]
mod tests {
    use risingwave_common::types::{DataType, IntervalUnit, NaiveDateTimeWrapper, ScalarImpl};

    use super::*;
    use crate::expr::{Expression, LiteralExpression};
    use crate::vector_op::cast::str_to_timestamp;

    const CHUNK_SIZE: usize = 1024;

    #[test]
    fn test_generate_i32_series() {
        generate_series_test_case(2, 4, 1);
        generate_series_test_case(4, 2, -1);
        generate_series_test_case(0, 9, 2);
        generate_series_test_case(0, (CHUNK_SIZE * 2 + 3) as i32, 1);
    }

    fn generate_series_test_case(start: i32, stop: i32, step: i32) {
        fn to_lit_expr(v: i32) -> BoxedExpression {
            LiteralExpression::new(DataType::Int32, Some(v.into())).boxed()
        }

        let function = GenerateSeries::<I32Array, I32Array, true>::new(
            to_lit_expr(start),
            to_lit_expr(stop),
            to_lit_expr(step),
            CHUNK_SIZE,
        )
        .boxed();
        let expect_cnt = ((stop - start) / step + 1) as usize;

        let dummy_chunk = DataChunk::new_dummy(1);
        let arrays = function.eval(&dummy_chunk).unwrap();

        let cnt: usize = arrays.iter().map(|a| a.len()).sum();
        assert_eq!(cnt, expect_cnt);
    }

    #[test]
    fn test_generate_time_series() {
        let start_time = str_to_timestamp("2008-03-01 00:00:00").unwrap();
        let stop_time = str_to_timestamp("2008-03-09 00:00:00").unwrap();
        let one_minute_step = IntervalUnit::from_minutes(1);
        let one_hour_step = IntervalUnit::from_minutes(60);
        let one_day_step = IntervalUnit::from_days(1);
        generate_time_series_test_case(start_time, stop_time, one_minute_step, 60 * 24 * 8 + 1);
        generate_time_series_test_case(start_time, stop_time, one_hour_step, 24 * 8 + 1);
        generate_time_series_test_case(start_time, stop_time, one_day_step, 8 + 1);
        generate_time_series_test_case(stop_time, start_time, -one_day_step, 8 + 1);
    }

    fn generate_time_series_test_case(
        start: NaiveDateTimeWrapper,
        stop: NaiveDateTimeWrapper,
        step: IntervalUnit,
        expect_cnt: usize,
    ) {
        fn to_lit_expr(ty: DataType, v: ScalarImpl) -> BoxedExpression {
            LiteralExpression::new(ty, Some(v)).boxed()
        }

        let function = GenerateSeries::<NaiveDateTimeArray, IntervalArray, true>::new(
            to_lit_expr(DataType::Timestamp, start.into()),
            to_lit_expr(DataType::Timestamp, stop.into()),
            to_lit_expr(DataType::Interval, step.into()),
            CHUNK_SIZE,
        );

        let dummy_chunk = DataChunk::new_dummy(1);
        let arrays = function.eval(&dummy_chunk).unwrap();

        let cnt: usize = arrays.iter().map(|a| a.len()).sum();
        assert_eq!(cnt, expect_cnt);
    }

    #[test]
    fn test_i32_range() {
        range_test_case(2, 4, 1);
        range_test_case(4, 2, -1);
        range_test_case(0, 9, 2);
        range_test_case(0, (CHUNK_SIZE * 2 + 3) as i32, 1);
    }

    fn range_test_case(start: i32, stop: i32, step: i32) {
        fn to_lit_expr(v: i32) -> BoxedExpression {
            LiteralExpression::new(DataType::Int32, Some(v.into())).boxed()
        }

        let function = GenerateSeries::<I32Array, I32Array, false>::new(
            to_lit_expr(start),
            to_lit_expr(stop),
            to_lit_expr(step),
            CHUNK_SIZE,
        )
        .boxed();
        let expect_cnt = ((stop - start - step.signum()) / step + 1) as usize;

        let dummy_chunk = DataChunk::new_dummy(1);
        let arrays = function.eval(&dummy_chunk).unwrap();

        let cnt: usize = arrays.iter().map(|a| a.len()).sum();
        assert_eq!(cnt, expect_cnt);
    }

    #[test]
    fn test_time_range() {
        let start_time = str_to_timestamp("2008-03-01 00:00:00").unwrap();
        let stop_time = str_to_timestamp("2008-03-09 00:00:00").unwrap();
        let one_minute_step = IntervalUnit::from_minutes(1);
        let one_hour_step = IntervalUnit::from_minutes(60);
        let one_day_step = IntervalUnit::from_days(1);
        time_range_test_case(start_time, stop_time, one_minute_step, 60 * 24 * 8);
        time_range_test_case(start_time, stop_time, one_hour_step, 24 * 8);
        time_range_test_case(start_time, stop_time, one_day_step, 8);
        time_range_test_case(stop_time, start_time, -one_day_step, 8);
    }

    fn time_range_test_case(
        start: NaiveDateTimeWrapper,
        stop: NaiveDateTimeWrapper,
        step: IntervalUnit,
        expect_cnt: usize,
    ) {
        fn to_lit_expr(ty: DataType, v: ScalarImpl) -> BoxedExpression {
            LiteralExpression::new(ty, Some(v)).boxed()
        }

        let function = GenerateSeries::<NaiveDateTimeArray, IntervalArray, false>::new(
            to_lit_expr(DataType::Timestamp, start.into()),
            to_lit_expr(DataType::Timestamp, stop.into()),
            to_lit_expr(DataType::Interval, step.into()),
            CHUNK_SIZE,
        );

        let dummy_chunk = DataChunk::new_dummy(1);
        let arrays = function.eval(&dummy_chunk).unwrap();

        let cnt: usize = arrays.iter().map(|a| a.len()).sum();
        assert_eq!(cnt, expect_cnt);
    }
}
