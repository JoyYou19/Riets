//Thanks claude:
//
//TODO need a smarter way to write test but yea now its in alphabetical order for
//create-insert-delete search policy status
//
//TODO duplicate primary key
//TODO primary key not given
//TODO visi parejie api calls

#[cfg(test)]
mod tests {
    const PORT: u16 = 6006;
    const TEST_DB_NAME: &str = "testdb";

    fn url(path: &str) -> String {
        format!("http://localhost:{PORT}{path}")
    }

    fn client() -> reqwest::Client {
        reqwest::Client::new()
    }

    async fn get_json(res: reqwest::Response) -> serde_json::Value {
        serde_json::from_str(&res.text().await.unwrap()).unwrap()
    }

    // -------------------------------------------------------------------------
    // CLEANUP
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_000_cleanup() {
        // delete the test db if it exists from a previous run
        client()
            .delete(url(&format!(
                "/api/databases/{TEST_DB_NAME}/delete-database"
            )))
            .send()
            .await
            .unwrap();
        // ignore the response — may 404 if it doesn't exist, that's fine
    }

    // -------------------------------------------------------------------------
    // CREATE
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_001_create_database_ok() {
        let res = client()
            .post(url(&format!(
                "/api/databases/{TEST_DB_NAME}/create-database"
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 201);
    }

    // -------------------------------------------------------------------------
    // STATUS (0 documents)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_002_status_empty_database() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/status")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        assert_eq!(json["data"]["document_count"], 0);
    }

    // -------------------------------------------------------------------------
    // CREATE DUPLICATE
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_003_create_database_duplicate() {
        let res = client()
            .post(url(&format!(
                "/api/databases/{TEST_DB_NAME}/create-database"
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 409);
    }

    // -------------------------------------------------------------------------
    // INSERT
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_004_insert_doc1_low_rust_score() {
        // fewer occurrences of "rust" — should rank lower
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body(r#"{"id":"1","title":"rust programming language","body":"systems programming safety"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_005_insert_doc2_high_rust_score() {
        // many occurrences of "rust" — should rank higher for query "rust"
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body(r#"{"id":"2","title":"rust rust rust","body":"rust rust rust rust rust"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_006_insert_batch_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body(r#"[{"id":"3","title":"search engines explained","body":"indexing and retrieval"},{"id":"4","title":"database storage","body":"modern databases store data on disk"}]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_007_insert_nested_json_ok() {
        // nested doc — meta/author should be searchable after policy is set
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body(r#"{"id":"5","title":"nested document","meta":{"author":"normunds","date":"2026"}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_008_insert_missing_id() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body(r#"{"title":"hello","body":"world"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_009_insert_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_010_insert_invalid_json() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/json")))
            .body("not json at all")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_011_insert_unsupported_filetype() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert/csv")))
            .body(r#"{"id":"4","title":"hello"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_012_insert_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/insert/json"))
            .body(r#"{"id":"1","title":"hello"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    // -------------------------------------------------------------------------
    // SEARCH
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_013_search_rust_ranking() {
        // id:2 has more "rust" occurrences so should be first hit
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/json")))
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0]["id"], "2", "id:2 should rank first for 'rust'");
    }

    #[tokio::test]
    async fn test_014_search_nested_field_before_policy() {
        // meta/author not in policy yet — normunds may not be found
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/json")))
            .body("normunds")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert_eq!(
            hits.len(),
            0,
            "normunds should not be found before policy is set"
        );
    }

    #[tokio::test]
    async fn test_015_search_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/json")))
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_016_search_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/search/json"))
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_017_search_unsupported_filetype() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/csv")))
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    // -------------------------------------------------------------------------
    // STATUS (with documents)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_018_status_with_documents() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/status")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        assert!(
            json["data"]["document_count"].as_u64().unwrap() > 0,
            "expected document_count > 0"
        );
    }

    #[tokio::test]
    async fn test_019_status_db_not_found() {
        let res = client()
            .get(url("/api/databases/nonexistent/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    // -------------------------------------------------------------------------
    // POLICY
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_020_get_policy_ok() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_021_get_policy_invalid_filetype() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/policy/csv")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_022_get_policy_db_not_found() {
        let res = client()
            .get(url("/api/databases/nonexistent/policy/json"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_023_set_valid_policy_with_nested_field() {
        // add meta/author to policy so it gets indexed and searched
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .body(
                r#"{
                "fields": [
                    {
                        "name": "title",
                        "xpath": 1,
                        "index": "Text",
                        "stored": true,
                        "stemming": "english",
                        "weight": { "min": 65, "max": 90 }
                    },
                    {
                        "name": "body",
                        "xpath": 2,
                        "index": "Text",
                        "stored": true,
                        "stemming": "english",
                        "weight": { "min": 1, "max": 75 }
                    },
                    {
                        "name": "meta/author",
                        "xpath": 3,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 1, "max": 75 }
                    }
                ]
            }"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_024_set_policy_duplicate_field_name() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .body(
                r#"{
                "fields": [
                    {
                        "name": "title",
                        "xpath": 1,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 1, "max": 90 }
                    },
                    {
                        "name": "title",
                        "xpath": 2,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 1, "max": 90 }
                    }
                ]
            }"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_025_set_policy_duplicate_xpath() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .body(
                r#"{
                "fields": [
                    {
                        "name": "title",
                        "xpath": 1,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 1, "max": 90 }
                    },
                    {
                        "name": "body",
                        "xpath": 1,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 1, "max": 90 }
                    }
                ]
            }"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_026_set_policy_bad_weight_range() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .body(
                r#"{
                "fields": [
                    {
                        "name": "title",
                        "xpath": 1,
                        "index": "Text",
                        "stored": true,
                        "stemming": null,
                        "weight": { "min": 90, "max": 1 }
                    }
                ]
            }"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_027_set_policy_invalid_json() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/json")))
            .body("not json at all")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_028_set_policy_unsupported_filetype() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy/csv")))
            .body(r#"{"fields":[]}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    // -------------------------------------------------------------------------
    // REINDEX (after policy change to include meta/author)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_029_reindex_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/reindex")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_030_reindex_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/reindex"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    // -------------------------------------------------------------------------
    // SEARCH after reindex (nested field now indexed)
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_031_search_rust_ranking_after_reindex() {
        // id:2 should still rank first for "rust"
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/json")))
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(
            hits[0]["id"], "2",
            "id:2 should still rank first after reindex"
        );
    }

    #[tokio::test]
    async fn test_032_search_nested_field_after_reindex() {
        // meta/author is now indexed — normunds should be found
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search/json")))
            .body("normunds")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(
            !hits.is_empty(),
            "normunds should be found after reindex with updated policy"
        );
        assert_eq!(hits[0]["id"], "5", "id:5 should be the hit for 'normunds'");
    }

    // -------------------------------------------------------------------------
    // RETRIEVE
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_033_retrieve_single_document_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["id"], "1");
    }

    #[tokio::test]
    async fn test_034_retrieve_multiple_documents_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body(r#"["1","2","3"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 3);
    }

    #[tokio::test]
    async fn test_035_retrieve_non_existent_document() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body(r#"["999999"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        let not_found = json["data"]["not_found"].as_array().unwrap();
        assert_eq!(docs.len(), 0);
        assert_eq!(not_found.len(), 1);
        assert_eq!(not_found[0], "999999");
    }

    #[tokio::test]
    async fn test_036_retrieve_mixed_existing_and_non_existent() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body(r#"["1","999999"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = get_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        let not_found = json["data"]["not_found"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(not_found.len(), 1);
        assert_eq!(docs[0]["id"], "1");
        assert_eq!(not_found[0], "999999");
    }

    #[tokio::test]
    async fn test_037_retrieve_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_038_retrieve_invalid_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/json")))
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_039_retrieve_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/retrieve/json"))
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_040_retrieve_unsupported_filetype() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve/csv")))
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    // -------------------------------------------------------------------------
    // DELETE
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn test_041_delete_database_not_found() {
        let res = client()
            .delete(url("/api/databases/nonexistent/delete-database"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_042_delete_database_ok() {
        let res = client()
            .delete(url(&format!(
                "/api/databases/{TEST_DB_NAME}/delete-database"
            )))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }
}
