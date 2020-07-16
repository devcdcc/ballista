// Copyright 2020 Andy Grove
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::arrow::datatypes::Schema;
use crate::arrow::record_batch::RecordBatch;
use crate::datafusion::execution::context::ExecutionContext as DFContext;
use crate::datafusion::logicalplan::LogicalPlan;
use crate::distributed::client::execute_action;
use crate::distributed::scheduler::{
    create_job, create_physical_plan, ensure_requirements, execute_job, ExecutionTask,
};
use crate::error::{ballista_error, Result};
use crate::execution::physical_plan::{
    Action, ColumnarBatch, ExecutionContext, ExecutorMeta, PhysicalPlan, ShuffleId,
};

use crate::distributed::etcd::{etcd_get_executors, start_etcd_thread};
use crate::distributed::k8s::k8s_get_executors;
use async_trait::async_trait;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    pub(crate) discovery_mode: DiscoveryMode,
    host: String,
    port: usize,
    etcd_urls: String,
}

impl ExecutorConfig {
    pub fn new(discovery_mode: DiscoveryMode, host: &str, port: usize, etcd_urls: &str) -> Self {
        Self {
            discovery_mode,
            host: host.to_owned(),
            port,
            etcd_urls: etcd_urls.to_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum DiscoveryMode {
    Etcd,
    Kubernetes,
    Standalone,
}

#[derive(Clone)]
pub struct ShufflePartition {
    pub(crate) schema: Schema,
    pub(crate) data: Vec<RecordBatch>,
}

#[async_trait]
pub trait Executor: Send + Sync {
    /// Execute a query and store the resulting shuffle partitions in memory
    async fn do_task(&self, task: &ExecutionTask) -> Result<ShuffleId>;

    /// Collect the results of a prior task that resulted in a shuffle partition
    fn collect(&self, shuffle_id: &ShuffleId) -> Result<ShufflePartition>;

    /// Execute a query and return results
    async fn execute_query(&self, plan: &LogicalPlan) -> Result<ShufflePartition>;
}

pub struct DefaultContext {
    /// map from shuffle id to executor uuid
    pub(crate) shuffle_locations: HashMap<ShuffleId, ExecutorMeta>,
    config: ExecutorConfig,
}

impl DefaultContext {
    pub fn new(
        config: &ExecutorConfig,
        shuffle_locations: HashMap<ShuffleId, ExecutorMeta>,
    ) -> Self {
        Self {
            config: config.clone(),
            shuffle_locations,
        }
    }
}

impl DefaultContext {}

#[async_trait]
impl ExecutionContext for DefaultContext {
    async fn get_executor_ids(&self) -> Result<Vec<ExecutorMeta>> {
        match &self.config.discovery_mode {
            DiscoveryMode::Etcd => etcd_get_executors(&self.config.etcd_urls, "default").await,
            DiscoveryMode::Kubernetes => k8s_get_executors("default", "default").await,
            DiscoveryMode::Standalone => unimplemented!(),
        }
    }

    async fn execute_task(
        &self,
        executor_meta: ExecutorMeta,
        task: ExecutionTask,
    ) -> Result<ShuffleId> {
        // TODO what is the point of returning this info since it is based on input arg?
        let shuffle_id = ShuffleId::new(task.job_uuid, task.stage_id, task.partition_id);

        let _ = execute_action(
            &executor_meta.host,
            executor_meta.port,
            Action::Execute(task),
        )
        .await?;

        Ok(shuffle_id)
    }

    async fn read_shuffle(&self, shuffle_id: &ShuffleId) -> Result<Vec<ColumnarBatch>> {
        match self.shuffle_locations.get(shuffle_id) {
            Some(executor_meta) => {
                let batches = execute_action(
                    &executor_meta.host,
                    executor_meta.port,
                    Action::FetchShuffle(*shuffle_id),
                )
                .await?;
                Ok(batches
                    .iter()
                    .map(|b| ColumnarBatch::from_arrow(b))
                    .collect())
            }
            _ => Err(ballista_error(&format!(
                "Failed to resolve executor UUID for shuffle ID {:?}",
                shuffle_id
            ))),
        }
    }
}

pub struct BallistaExecutor {
    config: ExecutorConfig,
    shuffle_partitions: Arc<Mutex<HashMap<String, ShufflePartition>>>,
}

impl BallistaExecutor {
    pub fn new(config: ExecutorConfig) -> Self {
        let uuid = Uuid::new_v4();

        match &config.discovery_mode {
            DiscoveryMode::Etcd => {
                println!("Running in etcd mode");
                start_etcd_thread(
                    &config.etcd_urls,
                    "default",
                    &uuid,
                    &config.host,
                    config.port,
                );
            }
            DiscoveryMode::Kubernetes => println!("Running in k8s mode"),
            DiscoveryMode::Standalone => println!("Running in standalone mode"),
        }

        Self {
            config,
            shuffle_partitions: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

#[async_trait]
impl Executor for BallistaExecutor {
    async fn do_task(&self, task: &ExecutionTask) -> Result<ShuffleId> {
        // create new execution contrext specifically for this query
        let ctx = Arc::new(DefaultContext::new(
            &self.config,
            task.shuffle_locations.clone(),
        ));

        let shuffle_id = ShuffleId::new(task.job_uuid, task.stage_id, task.partition_id);

        let exec_plan = task.plan.as_execution_plan();
        let stream = exec_plan.execute(ctx, task.partition_id).await?;
        let mut batches = vec![];
        while let Some(batch) = stream.next().await? {
            batches.push(batch.to_arrow()?);
        }

        let key = format!(
            "{}:{}:{}",
            shuffle_id.job_uuid, shuffle_id.stage_id, shuffle_id.partition_id
        );
        let mut shuffle_partitions = self.shuffle_partitions.lock().unwrap();
        shuffle_partitions.insert(
            key,
            ShufflePartition {
                schema: stream.schema().as_ref().clone(),
                data: batches,
            },
        );

        Ok(shuffle_id)
    }

    fn collect(&self, shuffle_id: &ShuffleId) -> Result<ShufflePartition> {
        let key = format!(
            "{}:{}:{}",
            shuffle_id.job_uuid, shuffle_id.stage_id, shuffle_id.partition_id
        );
        let shuffle_partitions = self.shuffle_partitions.lock().unwrap();
        match shuffle_partitions.get(&key) {
            Some(partition) => Ok(partition.clone()),
            _ => Err(ballista_error("invalid shuffle partition id")),
        }
    }

    async fn execute_query(&self, logical_plan: &LogicalPlan) -> Result<ShufflePartition> {
        println!("Logical plan:\n{:?}", logical_plan);
        let ctx = DFContext::new();
        let logical_plan = ctx.optimize(&logical_plan)?;
        println!("Optimized logical plan:\n{:?}", logical_plan);

        let config = self.config.clone();
        let handle = thread::spawn(move || {
            smol::run(async {
                let plan: Arc<PhysicalPlan> = create_physical_plan(&logical_plan)?;
                println!("Physical plan:\n{:?}", plan);

                let plan = ensure_requirements(plan.as_ref())?;
                println!("Optimized physical plan:\n{:?}", plan);

                let job = create_job(plan)?;
                job.explain();

                // create new execution contrext specifically for this query
                let ctx = Arc::new(DefaultContext::new(&config, HashMap::new()));

                let batches = execute_job(&job, ctx.clone()).await?;

                Ok(ShufflePartition {
                    schema: batches[0].schema().as_ref().clone(),
                    data: batches
                        .iter()
                        .map(|b| b.to_arrow())
                        .collect::<Result<Vec<_>>>()?,
                })
            })
        });
        handle.join().unwrap()
    }
}
