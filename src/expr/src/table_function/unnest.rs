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

use risingwave_common::array::{Array, ArrayRef, DataChunk, ListArray, ListRef};
use risingwave_common::util::iter_util::ZipEqDebug;

use super::*;

#[derive(Debug)]
pub struct Unnest {
    return_type: DataType,
    list: BoxedExpression,
    chunk_size: usize,
}

impl Unnest {
    fn eval_row(&self, list: ListRef<'_>) -> Result<ArrayRef> {
        let mut builder = self.return_type.create_array_builder(self.chunk_size);
        for d in &list.flatten() {
            builder.append_datum(*d);
        }
        Ok(Arc::new(builder.finish()))
    }
}

impl TableFunction for Unnest {
    fn return_type(&self) -> DataType {
        self.return_type.clone()
    }

    fn eval(&self, input: &DataChunk) -> Result<Vec<ArrayRef>> {
        let ret_list = self.list.eval_checked(input)?;
        let arr_list: &ListArray = ret_list.as_ref().into();

        let bitmap = input.visibility();
        let mut output_arrays: Vec<ArrayRef> = vec![];

        match bitmap {
            Some(bitmap) => {
                for (list, visible) in arr_list.iter().zip_eq_debug(bitmap.iter()) {
                    let array = if !visible {
                        empty_array(self.return_type())
                    } else if let Some(list) = list {
                        self.eval_row(list)?
                    } else {
                        empty_array(self.return_type())
                    };
                    output_arrays.push(array);
                }
            }
            None => {
                for list in arr_list.iter() {
                    let array = if let Some(list) = list {
                        self.eval_row(list)?
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

pub fn new_unnest(prost: &TableFunctionProst, chunk_size: usize) -> Result<BoxedTableFunction> {
    let return_type = DataType::from(prost.get_return_type().unwrap());
    let args: Vec<_> = prost.args.iter().map(expr_build_from_prost).try_collect()?;
    let [list]: [_; 1] = args.try_into().unwrap();

    Ok(Unnest {
        return_type,
        list,
        chunk_size,
    }
    .boxed())
}
