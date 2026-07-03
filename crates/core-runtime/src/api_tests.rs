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

    async fn body_json(res: reqwest::Response) -> serde_json::Value {
        serde_json::from_str(&res.text().await.unwrap()).unwrap()
    }

    // ── CLEANUP ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_000_cleanup() {
        client()
            .delete(url(&format!(
                "/api/databases/{TEST_DB_NAME}/delete-database"
            )))
            .send()
            .await
            .unwrap();
        // ignore response — may 404 if db doesn't exist from previous run
    }

    // ── CREATE ────────────────────────────────────────────────────────────────

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

    // ── STATUS (empty) ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_002_status_empty_database() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/status")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        assert_eq!(json["data"]["document_count"], 0);
    }

    // ── CREATE DUPLICATE ──────────────────────────────────────────────────────

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

    // ── INSERT ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_004_insert_doc1_low_rust_score() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body(r#"{"id":"1","title":"rust programming language","body":"systems programming safety"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_005_insert_doc2_high_rust_score() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body(r#"{"id":"2","title":"rust rust rust","body":"rust rust rust rust rust"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_006_insert_batch_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body(r#"[{"id":"3","title":"search engines explained","body":"indexing and retrieval"},{"id":"4","title":"database storage","body":"modern databases store data on disk"}]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_007_insert_nested_json_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body(r#"{"id":"5","title":"nested document","meta":{"author":"normunds","date":"2026"}}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_008_insert_missing_id() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body(r#"{"title":"hello","body":"world"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_009_insert_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_010_insert_invalid_json() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/json")
            .body("not json at all")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_011_insert_unsupported_format() {
        // unsupported Accept header — caught by middleware before handler
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .header("Accept", "application/csv")
            .body(r#"{"id":"4","title":"hello"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 406);
    }

    #[tokio::test]
    async fn test_012_insert_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/insert"))
            .header("Accept", "application/json")
            .body(r#"{"id":"1","title":"hello"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_013_insert_no_accept_uses_default() {
        // no Accept header — should use config default (json)
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/insert")))
            .body(r#"{"id":"6","title":"default format insert","body":"no accept header"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // ── SEARCH ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_014_search_rust_ranking() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/json")
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(!hits.is_empty(), "expected at least one hit");
        assert_eq!(hits[0]["id"], "2", "id:2 should rank first for 'rust'");
    }

    #[tokio::test]
    async fn test_015_search_nested_field_before_policy() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/json")
            .body("normunds")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert_eq!(
            hits.len(),
            0,
            "normunds should not be found before policy is set"
        );
    }

    #[tokio::test]
    async fn test_016_search_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/json")
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_017_search_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/search"))
            .header("Accept", "application/json")
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_018_search_unsupported_format() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/csv")
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 406);
    }

    #[tokio::test]
    async fn test_019_search_no_accept_uses_default() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // ── STATUS (with documents) ───────────────────────────────────────────────

    #[tokio::test]
    async fn test_020_status_with_documents() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/status")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        assert!(json["data"]["document_count"].as_u64().unwrap() > 0);
    }

    #[tokio::test]
    async fn test_021_status_db_not_found() {
        let res = client()
            .get(url("/api/databases/nonexistent/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    // ── POLICY ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_022_get_policy_ok() {
        // policy is always TOML regardless of Accept header
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/toml"
        );
    }

    #[tokio::test]
    async fn test_023_get_policy_unsupported_format() {
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .header("Accept", "application/csv")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 406);
    }

    #[tokio::test]
    async fn test_024_get_policy_db_not_found() {
        let res = client()
            .get(url("/api/databases/nonexistent/policy"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_025_get_policy_accept_xml_still_returns_toml() {
        // policy bypasses format resolution — always TOML
        let res = client()
            .get(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .header("Accept", "application/xml")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        assert_eq!(
            res.headers().get("content-type").unwrap(),
            "application/toml"
        );
    }

    #[tokio::test]
    async fn test_026_set_valid_policy_with_nested_field() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body(
                r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90

[[fields]]
name     = "body"
xpath    = 2
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 1
max = 75

[[fields]]
name     = "meta/author"
xpath    = 3
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 1
max = 75
"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_027_set_policy_duplicate_field_name() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body(
                r#"
[[fields]]
name  = "title"
xpath = 1
index = "Text"
stored = true

[fields.weight]
min = 1
max = 90

[[fields]]
name  = "title"
xpath = 2
index = "Text"
stored = true

[fields.weight]
min = 1
max = 90
"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_028_set_policy_duplicate_xpath() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body(
                r#"
[[fields]]
name  = "title"
xpath = 1
index = "Text"
stored = true

[fields.weight]
min = 1
max = 90

[[fields]]
name  = "body"
xpath = 1
index = "Text"
stored = true

[fields.weight]
min = 1
max = 90
"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_029_set_policy_bad_weight_range() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body(
                r#"
[[fields]]
name  = "title"
xpath = 1
index = "Text"
stored = true

[fields.weight]
min = 90
max = 1
"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_030_set_policy_invalid_toml() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body("this is ][ not toml")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_031_set_policy_unsupported_format() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .header("Accept", "application/csv")
            .body("[[fields]]")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 406);
    }

    #[tokio::test]
    async fn test_032_set_policy_no_accept_uses_default() {
        // no Accept header — middleware uses default, policy body is always TOML
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/policy")))
            .body(
                r#"
[[fields]]
name     = "title"
xpath    = 1
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 65
max = 90

[[fields]]
name     = "body"
xpath    = 2
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 1
max = 75

[[fields]]
name     = "meta/author"
xpath    = 3
index    = "Text"
stored   = true
stemming = "english"

[fields.weight]
min = 1
max = 75
"#,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // ── REINDEX ───────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_033_reindex_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/reindex")))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    #[tokio::test]
    async fn test_034_reindex_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/reindex"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    // ── SEARCH after reindex ──────────────────────────────────────────────────

    #[tokio::test]
    async fn test_035_search_rust_ranking_after_reindex() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/json")
            .body("rust")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(!hits.is_empty());
        assert_eq!(
            hits[0]["id"], "2",
            "id:2 should still rank first after reindex"
        );
    }

    #[tokio::test]
    async fn test_036_search_nested_field_after_reindex() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/search")))
            .header("Accept", "application/json")
            .body("normunds")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let hits = json["data"].as_array().unwrap();
        assert!(
            !hits.is_empty(),
            "normunds should be found after reindex with updated policy"
        );
        assert_eq!(hits[0]["id"], "5");
    }

    // ── RETRIEVE ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_037_retrieve_single_document_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0]["id"], "1");
    }

    #[tokio::test]
    async fn test_038_retrieve_multiple_documents_ok() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body(r#"["1","2","3"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        assert_eq!(docs.len(), 3);
    }

    #[tokio::test]
    async fn test_039_retrieve_non_existent_document() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body(r#"["999999"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        let not_found = json["data"]["not_found"].as_array().unwrap();
        assert_eq!(docs.len(), 0);
        assert_eq!(not_found.len(), 1);
        assert_eq!(not_found[0], "999999");
    }

    #[tokio::test]
    async fn test_040_retrieve_mixed_existing_and_non_existent() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body(r#"["1","999999"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let json = body_json(res).await;
        let docs = json["data"]["documents"].as_array().unwrap();
        let not_found = json["data"]["not_found"].as_array().unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(not_found.len(), 1);
        assert_eq!(docs[0]["id"], "1");
        assert_eq!(not_found[0], "999999");
    }

    #[tokio::test]
    async fn test_041_retrieve_empty_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body("")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_042_retrieve_invalid_body() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/json")
            .body("not json")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn test_043_retrieve_db_not_found() {
        let res = client()
            .post(url("/api/databases/nonexistent/retrieve"))
            .header("Accept", "application/json")
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_044_retrieve_unsupported_format() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .header("Accept", "application/csv")
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 406);
    }

    #[tokio::test]
    async fn test_045_retrieve_no_accept_uses_default() {
        let res = client()
            .post(url(&format!("/api/databases/{TEST_DB_NAME}/retrieve")))
            .body(r#"["1"]"#)
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
    }

    // ── DELETE ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn test_046_delete_database_not_found() {
        let res = client()
            .delete(url("/api/databases/nonexistent/delete-database"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn test_047_delete_database_ok() {
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
