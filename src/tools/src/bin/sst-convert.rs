// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! A cli to convert ssts between different options

use std::sync::Arc;

use analytic_engine::{
    prefetchable_stream::PrefetchableStreamExt,
    sst::{
        factory::{
            Factory, FactoryImpl, ObjectStorePickerRef, ReadFrequency, ScanOptions, SstReadHint,
            SstReadOptions, SstWriteOptions,
        },
        file::Level,
        metrics::MaybeTableLevelMetrics as SstMaybeTableLevelMetrics,
    },
    table_options::{Compression, StorageFormatHint},
};
use anyhow::{Context, Result};
use clap::Parser;
use common_types::{
    projected_schema::{ProjectedSchema, RowProjectorBuilder},
    request_id::RequestId,
};
use generic_error::BoxError;
use object_store::{LocalFileSystem, Path};
use runtime::Runtime;
use table_engine::predicate::Predicate;
use tools::sst_util;

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Args {
    /// Root dir of storage
    #[clap(short, long, required(true))]
    store_path: String,

    /// Input sst file(relative to store_path)
    #[clap(short, long, required(true))]
    input: String,

    /// Output sst file(relative to store_path)
    #[clap(short, long, required(true))]
    output: String,

    /// Compression of new sst file(values: uncompressed/lz4/snappy/zstd)
    #[clap(short, long, default_value = "zstd")]
    compression: String,

    /// Row group size of new sst file
    #[clap(short, long, default_value_t = 8192)]
    batch_size: usize,

    /// Storage format(values: columnar)
    #[clap(short, long, default_value = "columnar")]
    output_format: String,
}

fn new_runtime(thread_num: usize) -> Runtime {
    runtime::Builder::default()
        .thread_name("tools")
        .worker_threads(thread_num)
        .enable_all()
        .build()
        .unwrap()
}

fn main() {
    let args = Args::parse();
    let rt = Arc::new(new_runtime(4));
    let rt2 = rt.clone();
    rt.block_on(async move {
        if let Err(e) = run(args, rt2).await {
            eprintln!("Convert failed, err:{e}");
        }
    });
}

async fn run(args: Args, runtime: Arc<Runtime>) -> Result<()> {
    let storage = LocalFileSystem::new_with_prefix(args.store_path).expect("invalid path");
    let store = Arc::new(storage) as _;
    let input_path = Path::from(args.input);
    let sst_meta = sst_util::meta_from_sst(&store, &input_path).await;
    let factory = FactoryImpl;
    let scan_options = ScanOptions::default();
    let projected_schema = ProjectedSchema::no_projection(sst_meta.schema.clone());

    let fetched_schema = projected_schema.to_record_schema();
    let table_schema = projected_schema.table_schema().clone();
    let row_projector_builder = RowProjectorBuilder::new(fetched_schema, table_schema, None);
    let reader_opts = SstReadOptions {
        maybe_table_level_metrics: Arc::new(SstMaybeTableLevelMetrics::new("tool", "")),
        frequency: ReadFrequency::Once,
        num_rows_per_row_group: 8192,
        predicate: Arc::new(Predicate::empty()),
        meta_cache: None,
        scan_options,
        runtime,
        row_projector_builder,
    };
    let store_picker: ObjectStorePickerRef = Arc::new(store);
    let mut reader = factory
        .create_reader(
            &input_path,
            &reader_opts,
            SstReadHint::default(),
            &store_picker,
            None,
        )
        .await
        .expect("no sst reader found");

    let output_format_hint = StorageFormatHint::try_from(args.output_format.as_str())
        .with_context(|| format!("invalid storage format:{}", args.output_format))?;
    let builder_opts = SstWriteOptions {
        storage_format_hint: output_format_hint,
        num_rows_per_row_group: args.batch_size,
        compression: Compression::parse_from(&args.compression)
            .with_context(|| format!("invalid compression:{}", args.compression))?,
        max_buffer_size: 10 * 1024 * 1024,
        column_stats: Default::default(),
    };
    let output = Path::from(args.output);
    let mut writer = factory
        .create_writer(&builder_opts, &output, &store_picker, Level::MAX)
        .await
        .expect("no sst writer found");
    let sst_stream = reader.read().await.unwrap().map(BoxError::box_err);
    let sst_stream = sst_stream.into_boxed_stream();
    let sst_info = writer
        .write(RequestId::next_id(), &sst_meta, sst_stream)
        .await?;

    println!("Write success, info:{sst_info:?}");

    Ok(())
}
