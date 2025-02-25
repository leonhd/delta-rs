#[cfg(feature = "s3")]
#[allow(dead_code)]
mod s3_common;

#[allow(dead_code)]
mod fs_common;

use deltalake::{action, DeltaTable};
use std::collections::HashMap;
use std::future::Future;
use std::iter::FromIterator;
use std::time::Duration;

#[tokio::test]
#[cfg(feature = "s3")]
async fn concurrent_writes_s3() {
    s3_common::setup_dynamodb("concurrent_writes");
    s3_common::cleanup_dir_except(
        "s3://deltars/concurrent_workers/_delta_log",
        vec!["00000000000000000000.json".to_string()],
    )
    .await;
    run_test(|name| Worker::new("s3://deltars/concurrent_workers", name)).await;
}

/// An Azure Data Lake Gen2 Storage Account is required to run this test and must be provided by
/// the developer. Because of this requirement, the test cannot run in CI and is therefore marked
/// #[ignore]. As a result, the developer must execute these tests on their machine.
/// In order to execute tests, remove the #[ignore] below and execute via:
/// 'cargo test concurrent_writes_azure --features azure --test concurrent_writes_test -- --nocapture --exact'
/// `AZURE_STORAGE_ACCOUNT_NAME` is required to be set in the environment.
/// `AZURE_STORAGE_ACCOUNT_KEY` is required to be set in the environment.
#[ignore]
#[tokio::test]
#[cfg(feature = "azure")]
async fn concurrent_writes_azure() {
    use azure_storage::storage_shared_key_credential::StorageSharedKeyCredential;
    use azure_storage_datalake::clients::DataLakeClient;
    use chrono::Utc;
    use deltalake::DeltaTableConfig;
    use deltalake::{DeltaTableMetaData, Schema, SchemaDataType, SchemaField};
    use std::env;

    // Arrange
    let storage_account_name = env::var("AZURE_STORAGE_ACCOUNT_NAME").unwrap();
    let storage_account_key = env::var("AZURE_STORAGE_ACCOUNT_KEY").unwrap();

    let data_lake_client = DataLakeClient::new(
        StorageSharedKeyCredential::new(
            storage_account_name.to_owned(),
            storage_account_key.to_owned(),
        ),
        None,
    );

    // Create a new file system for test isolation
    let file_system_name = format!("test-delta-table-{}", Utc::now().timestamp());
    let file_system_client = data_lake_client.into_file_system_client(file_system_name.to_owned());
    file_system_client.create().into_future().await.unwrap();

    let table_uri = &format!("adls2://{}/{}/", storage_account_name, file_system_name);
    let backend = deltalake::get_backend_for_uri(table_uri).unwrap();
    let mut dt = DeltaTable::new(table_uri, backend, DeltaTableConfig::default()).unwrap();

    let schema = Schema::new(vec![SchemaField::new(
        "Id".to_string(),
        SchemaDataType::primitive("integer".to_string()),
        true,
        HashMap::new(),
    )]);

    let metadata = DeltaTableMetaData::new(
        Some("Azure Test Table".to_string()),
        None,
        None,
        schema,
        vec![],
        HashMap::new(),
    );

    let protocol = action::Protocol {
        min_reader_version: 1,
        min_writer_version: 2,
    };

    dt.create(metadata.clone(), protocol.clone(), None, None)
        .await
        .unwrap();

    assert_eq!(0, dt.version());
    assert_eq!(1, dt.get_min_reader_version());
    assert_eq!(2, dt.get_min_writer_version());
    assert_eq!(0, dt.get_files().len());
    assert_eq!(table_uri.trim_end_matches('/').to_string(), dt.table_uri);

    // Act/Assert
    run_test(|name| Worker::new(table_uri, name)).await;

    // Cleanup
    file_system_client.delete().into_future().await.unwrap();
}

#[tokio::test]
async fn concurrent_writes_fs() {
    prepare_fs();
    run_test(|name| Worker::new("./tests/data/concurrent_workers", name)).await;
}

const WORKERS: i64 = 5;
const COMMITS: i64 = 3;

async fn run_test<F, Fut>(create_worker: F)
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Worker>,
{
    let mut workers = Vec::new();
    for w in 0..WORKERS {
        workers.push(create_worker(format!("w{}", w)).await);
    }

    let mut futures = Vec::new();
    for mut w in workers {
        let run = tokio::spawn(async move { w.commit_sequence(COMMITS).await });
        futures.push(run)
    }

    let mut map = HashMap::new();
    for f in futures {
        map.extend(f.await.unwrap());
    }

    // to ensure that there's been no collisions between workers of acquiring the same version
    assert_eq!(map.len() as i64, WORKERS * COMMITS);

    // check that we have unique and ascending versions committed
    let mut versions = Vec::from_iter(map.keys().map(|x| x.clone()));
    versions.sort();
    assert_eq!(versions, Vec::from_iter(1i64..=WORKERS * COMMITS));

    // check that each file for each worker is committed as expected
    let mut files = Vec::from_iter(map.values().map(|x| x.clone()));
    files.sort();
    let mut expected = Vec::new();
    for w in 0..WORKERS {
        for c in 0..COMMITS {
            expected.push(format!("w{}-{}", w, c))
        }
    }
    assert_eq!(files, expected);
}

pub struct Worker {
    pub table: DeltaTable,
    pub name: String,
}

impl Worker {
    pub async fn new(path: &str, name: String) -> Self {
        std::env::set_var("DYNAMO_LOCK_OWNER_NAME", &name);
        let table = deltalake::open_table(path).await.unwrap();
        Self { table, name }
    }

    async fn commit_sequence(&mut self, n: i64) -> HashMap<i64, String> {
        let mut result = HashMap::new();
        for i in 0..n {
            let name = format!("{}-{}", self.name, i);
            let v = self.commit_file(&name).await;
            result.insert(v, name);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        result
    }

    async fn commit_file(&mut self, name: &str) -> i64 {
        let mut tx = self.table.create_transaction(None);
        tx.add_action(action::Action::add(action::Add {
            path: format!("{}.parquet", name),
            size: 396,
            partition_values: HashMap::new(),
            partition_values_parsed: None,
            modification_time: 1564524294000,
            data_change: true,
            stats: None,
            stats_parsed: None,
            tags: None,
        }));
        tx.commit(None, None).await.unwrap()
    }
}

fn prepare_fs() {
    fs_common::cleanup_dir_except(
        "./tests/data/concurrent_workers/_delta_log",
        vec!["00000000000000000000.json".to_string()],
    );
}
