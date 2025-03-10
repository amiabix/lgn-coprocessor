use std::collections::HashMap;

use anyhow::bail;
use anyhow::Context;
use lgn_messages::types::v1::query::tasks::QueryStep;
use lgn_messages::types::v1::query::tasks::RevelationInput;
use lgn_messages::types::v1::query::ConcreteQueryCircuitInput;
use lgn_messages::types::v1::query::ConcreteQueryParameters;
use lgn_messages::types::v1::query::WorkerTaskType;
use lgn_messages::types::MessageReplyEnvelope;
use lgn_messages::types::TaskType;
use lgn_messages::Proof;
use metrics::histogram;
use parsil::assembler::DynamicCircuitPis;
use tracing::info;
use verifiable_db::api::QueryCircuitInput;
use verifiable_db::query::api::CircuitInput;
use verifiable_db::query::computational_hash_ids::ColumnIDs;
use verifiable_db::query::universal_circuit::universal_circuit_inputs::Placeholders;
use verifiable_db::revelation;
use verifiable_db::revelation::api::MatchingRow;

use crate::params;
use crate::provers::LgnProver;

pub struct EuclidQueryProver {
    params: ConcreteQueryParameters,
}

impl EuclidQueryProver {
    pub fn new(params: ConcreteQueryParameters) -> Self {
        Self { params }
    }

    pub fn init(
        url: &str,
        dir: &str,
        file: &str,
        checksums: &HashMap<String, blake3::Hash>,
    ) -> anyhow::Result<Self> {
        let params = params::prepare_raw(url, dir, file, checksums)
            .context("while loading bincode-serialized parameters")?;
        let reader = std::io::BufReader::new(params.as_ref());
        let params = bincode::deserialize_from(reader)?;
        Ok(Self { params })
    }
}

impl EuclidQueryProver {
    fn prove_circuit_input(
        &self,
        circuit_input: ConcreteQueryCircuitInput,
    ) -> anyhow::Result<Proof> {
        info!("Proving query circuit");
        let now = std::time::Instant::now();

        let proof = self
            .params
            .generate_proof(circuit_input)
            .context("while generating proof for the universal circuit")?;

        let time = now.elapsed().as_secs_f32();
        histogram!("zkmr_worker_proving_latency", "proof_type" => "circuit_input").record(time);

        info!(
            "Query circuit. proof_size_kb: {} time: {:?}",
            proof.len() / 1024,
            now.elapsed()
        );

        Ok(proof)
    }

    #[allow(clippy::too_many_arguments)]
    fn prove_tabular_revelation(
        &self,
        pis: &DynamicCircuitPis,
        placeholders: Placeholders,
        indexing_proof: Vec<u8>,
        matching_rows: Vec<MatchingRow>,
        column_ids: &ColumnIDs,
        limit: u32,
        offset: u32,
    ) -> anyhow::Result<Vec<u8>> {
        let circuit_input = revelation::api::CircuitInput::new_revelation_tabular(
            indexing_proof,
            matching_rows,
            &pis.bounds,
            &placeholders,
            column_ids,
            &pis.predication_operations,
            &pis.result,
            limit,
            offset,
        )
        .context("while initializing the (empty) revelation circuit")?;

        let proof = self.prove_circuit_input(QueryCircuitInput::Revelation(circuit_input))?;

        Ok(proof)
    }

    pub fn run_inner(
        &self,
        task_type: WorkerTaskType,
    ) -> anyhow::Result<Vec<u8>> {
        let WorkerTaskType::Query(input) = task_type;

        let pis: DynamicCircuitPis = serde_json::from_slice(&input.pis)?;

        let final_proof = match input.query_step {
            QueryStep::Tabular(rows_inputs, revelation_input) => {
                let RevelationInput::Tabular {
                    placeholders,
                    indexing_proof,
                    matching_rows,
                    column_ids,
                    limit,
                    offset,
                    ..
                } = revelation_input;

                let mut matching_rows_proofs = vec![];
                for (row_input, matching_row) in rows_inputs.iter().zip(matching_rows) {
                    let circuit_input = CircuitInput::new_universal_circuit(
                        &row_input.column_cells,
                        &pis.predication_operations,
                        &pis.result,
                        &((&row_input.placeholders).into()),
                        row_input.is_leaf,
                        &pis.bounds,
                    )
                    .context("while initializing the universal circuit")?;

                    let proof =
                        self.prove_circuit_input(QueryCircuitInput::Query(circuit_input))?;
                    matching_rows_proofs.push(matching_row.hydrate(proof));
                }

                self.prove_tabular_revelation(
                    &pis,
                    placeholders.into(),
                    indexing_proof.clone_proof(),
                    matching_rows_proofs,
                    &column_ids,
                    limit,
                    offset,
                )?
            },
            QueryStep::QueryCircuitInput(circuit_input) => {
                self.prove_circuit_input(*circuit_input)?
            },
        };

        Ok(final_proof)
    }
}

impl LgnProver for EuclidQueryProver {
    fn run(
        &self,
        envelope: lgn_messages::types::MessageEnvelope,
    ) -> anyhow::Result<lgn_messages::types::MessageReplyEnvelope> {
        let task_id = envelope.task_id.clone();

        match envelope.task {
            TaskType::V1Preprocessing(..) => {
                bail!(
                "EuclidQueryProver: unsupported task type. task_type: V1Preprocessing task_id: {}",
                task_id,
            )
            },
            TaskType::V1Query(task_type) => {
                let proof = self.run_inner(task_type)?;
                Ok(MessageReplyEnvelope::new(task_id, proof))
            },
            TaskType::V1Groth16(..) => {
                bail!(
                    "EuclidQueryProver: unsupported task type. task_type: V1Groth16 task_id: {}",
                    task_id,
                )
            },
        }
    }
}
