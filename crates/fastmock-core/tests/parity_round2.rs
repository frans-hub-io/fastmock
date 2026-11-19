use bytes::Bytes;
use fastmock_core::matcher::CompiledStub;
use fastmock_core::template::Templates;
use fastmock_core::{CapturedRequest, Stub, mockserver, wiremock};
use serde_json::{Value, json};

fn post(path: &str, body: &Value) -> CapturedRequest {
    CapturedRequest::new(
        "POST",
        path,
        None,
        vec![("content-type".into(), "application/json".into())],
        Bytes::from(body.to_string()),
    )
}

fn hit(s: &Stub, req: &CapturedRequest) -> bool {
    let c = CompiledStub::compile(s).unwrap();
    let json = req.json().cloned();
    c.match_request(req, json.as_ref()).is_some()
}

fn wm_one(mapping: Value) -> (Option<Stub>, Vec<String>) {
    let c = wiremock::convert(&mapping);
    (c.stubs.into_iter().next(), c.warnings)
}

fn first_match<'a>(stubs: &'a [Stub], req: &CapturedRequest) -> Option<&'a Stub> {
    let mut compiled: Vec<CompiledStub> = stubs
        .iter()
        .map(|s| CompiledStub::compile(s).unwrap())
        .collect();
    compiled.sort_by_key(fastmock_core::engine::stub_rank);
    let id = compiled
        .iter()
        .find(|c| c.match_request(req, req.json()).is_some())?
        .stub
        .id;
    stubs.iter().find(|s| s.id == id)
}

#[test]
fn d1_matches_json_path_with_json_and_schema_sub_matchers() {
    let (s, w) = wm_one(json!({"request": {"urlPath": "/o", "bodyPatterns": [
        {"matchesJsonPath": {"expression": "$.items[0]", "equalToJson": "{\"sku\":\"A\"}"}}]},
        "response": {"body": "ok"}}));
    assert!(w.is_empty(), "{w:?}");
    let s = s.unwrap();
    assert!(!hit(&s, &post("/o", &json!({"items": [{"sku": "B"}]}))));
    assert!(hit(&s, &post("/o", &json!({"items": [{"sku": "A"}]}))));

    let (s, w) = wm_one(json!({"request": {"urlPath": "/o", "bodyPatterns": [
        {"matchesJsonPath": {"expression": "$.order", "equalToJson": {"id": 1}, "ignoreExtraElements": true}}]}}));
    assert!(w.is_empty(), "{w:?}");
    let s = s.unwrap();
    assert!(hit(&s, &post("/o", &json!({"order": {"id": 1, "x": 2}}))));
    assert!(!hit(&s, &post("/o", &json!({"order": {"id": 2}}))));

    let (s, _) = wm_one(json!({"request": {"urlPath": "/o", "bodyPatterns": [
        {"matchesJsonPath": {"expression": "$.order", "matchesJsonSchema": {"type": "object", "required": ["id"]}}}]}}));
    let s = s.unwrap();
    assert!(hit(&s, &post("/o", &json!({"order": {"id": 1}}))));
    assert!(!hit(&s, &post("/o", &json!({"order": {"name": "x"}}))));

    let (s, w) = wm_one(json!({"request": {"urlPath": "/o", "bodyPatterns": [
        {"matchesJsonPath": {"expression": "$.order", "equalToJson": "{}", "placeholderOpeningDelimiterRegex": "\\["}}]}}));
    assert!(
        s.is_none(),
        "an unsupported sub-matcher must not widen the stub"
    );
    assert!(w.iter().any(|x| x.contains("skipped")), "{w:?}");
}

#[test]
fn d2_structurally_invalid_documents_produce_no_stubs() {
    for doc in [
        json!({"tag": "keepme", "mappings": 5}),
        json!({"mapings": [{"request": {"urlPath": "/keep"}}]}),
        json!(5),
    ] {
        let c = wiremock::convert(&doc);
        assert!(c.stubs.is_empty(), "{doc}");
        assert!(!c.warnings.is_empty(), "{doc}");
    }
    let c =
        wiremock::convert(&json!({"mappings": [{"request": {"urlPath": "/a"}}, {"response": {}}]}));
    assert_eq!(c.stubs.len(), 1, "partial imports keep the valid mappings");
    assert!(c.warnings.iter().any(|w| w.contains("no request")));
    let c = mockserver::convert(&json!({"expectations": 5}));
    assert!(c.stubs.is_empty() && !c.warnings.is_empty());
    let c = mockserver::convert(&json!([{"httpRequest": {"path": "/v"},
        "httpResponseTemplate": {"templateType": "VELOCITY", "template": "{}"}}]));
    assert!(c.stubs.is_empty() && !c.warnings.is_empty());
}

#[test]
fn d4_extra_array_items_for_wiremock_and_mockserver() {
    let (s, _) = wm_one(json!({"request": {"urlPath": "/j", "bodyPatterns": [
        {"equalToJson": {"a": 1, "arr": [1, 2]}, "ignoreExtraElements": true}]}}));
    let s = s.unwrap();
    assert!(hit(
        &s,
        &post("/j", &json!({"a": 1, "arr": [1, 2, 3], "b": 2}))
    ));
    assert!(
        !hit(&s, &post("/j", &json!({"a": 1, "arr": [2, 1, 3]}))),
        "order still counts"
    );
    assert!(!hit(&s, &post("/j", &json!({"a": 1, "arr": [1]}))));

    let c = mockserver::convert(
        &json!([{"httpRequest": {"path": "/m", "body": {"type": "JSON", "json": {"a": 1, "arr": [1, 2]}}},
        "httpResponse": {"statusCode": 200}}]),
    );
    let s = &c.stubs[0];
    assert!(hit(s, &post("/m", &json!({"arr": [2, 1, 3], "a": 1}))));
    let c = mockserver::convert(
        &json!([{"httpRequest": {"path": "/m", "body": {"type": "JSON", "matchType": "STRICT", "json": {"arr": [1, 2]}}},
        "httpResponse": {"statusCode": 200}}]),
    );
    assert!(!hit(&c.stubs[0], &post("/m", &json!({"arr": [1, 2, 3]}))));
}

fn render(template: &str, req: &CapturedRequest, params: &[(&str, &str)]) -> String {
    let params = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Templates::new()
        .render(template, &req.template_context(&params, true))
        .unwrap()
}

#[test]
fn d5_d7_d8_wiremock_template_names() {
    let t = wiremock::convert_templates(
        "id={{request.path.id}} first={{request.path.[0]}} {{request.path.[contactId]}}",
    );
    assert_eq!(
        t,
        "id={{request.pathParams.[id]}} first={{request.pathSegments.[0]}} {{request.pathParams.[contactId]}}"
    );
    let req = CapturedRequest::new("GET", "/users/42", None, vec![], Bytes::new());
    assert_eq!(
        render(
            &wiremock::convert_templates("id={{request.path.id}}"),
            &req,
            &[("id", "42")]
        ),
        "id=42"
    );

    let t = wiremock::convert_templates(
        "{{originalRequest.headers.X-Trace}} {{originalRequest.path.[0]}} {{originalRequest.query.x.[1]}}",
    );
    assert_eq!(
        t,
        "{{originalRequest.headers.[x-trace]}} {{originalRequest.pathSegments.[0]}} {{originalRequest.queryAll.x.[1]}}"
    );
    let req = CapturedRequest::new(
        "GET",
        "/hooks",
        Some("x=1&x=2"),
        vec![("x-trace".into(), "tr-9".into())],
        Bytes::new(),
    );
    assert_eq!(render(&t, &req, &[]), "tr-9 hooks 2");

    let t = wiremock::convert_templates(
        "{{#each request.query.ids}}{{this}},{{/each}} {{size request.query.ids}} {{request.headers.X-Multi.[0]}}",
    );
    let req = CapturedRequest::new(
        "GET",
        "/m",
        Some("ids=1&ids=2"),
        vec![
            ("x-multi".into(), "a".into()),
            ("x-multi".into(), "b".into()),
        ],
        Bytes::new(),
    );
    assert_eq!(render(&t, &req, &[]), "1,2, 2 a");
}

#[test]
fn d6_json_path_helper_renders_json_text() {
    let req = post(
        "/x",
        &json!({"o": {"k": 1}, "items": [{"a": 1}, {"a": 2}], "s": "t"}),
    );
    assert_eq!(
        render("{{jsonPath request.body '$.o'}}", &req, &[]),
        r#"{"k":1}"#
    );
    assert_eq!(
        render("{{jsonPath request.body '$.items'}}", &req, &[]),
        r#"[{"a":1},{"a":2}]"#
    );
    assert_eq!(render("{{jsonPath request.body '$.s'}}", &req, &[]), "t");
    assert_eq!(
        render(
            "{{#each (jsonPath request.body '$.items')}}{{a}}{{/each}}",
            &req,
            &[]
        ),
        "12",
        "sub-expressions still get JSON values"
    );
}

#[test]
fn d9_absent_json_path() {
    let (s, _) = wm_one(json!({"request": {"urlPath": "/a", "bodyPatterns": [
        {"matchesJsonPath": {"expression": "$.missing", "absent": true}}]}}));
    let s = s.unwrap();
    assert!(hit(&s, &post("/a", &json!({"a": 1}))));
    assert!(!hit(&s, &post("/a", &json!({"missing": 1}))));
}

#[test]
fn d10_tie_breaks_follow_the_source_tool() {
    let req = CapturedRequest::new("GET", "/spec/x", None, vec![], Bytes::new());
    let c = wiremock::convert(&json!({"mappings": [
        {"request": {"urlPathPattern": "/spec/.*"}, "response": {"body": "pattern-first"}},
        {"request": {"urlPath": "/spec/x"}, "response": {"body": "exact-second"}}]}));
    let won = first_match(&c.stubs, &req).unwrap();
    assert_eq!(
        won.response.body.as_deref(),
        Some("pattern-first"),
        "like POST /__admin/mappings/import: URL specificity is not used, the first mapping wins"
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let later = wiremock::convert(
        &json!({"request": {"urlPathPattern": "/spec/.*"}, "response": {"body": "newer"}}),
    );
    let all: Vec<Stub> = c.stubs.iter().chain(later.stubs.iter()).cloned().collect();
    assert_eq!(
        first_match(&all, &req).unwrap().response.body.as_deref(),
        Some("newer")
    );

    let req = CapturedRequest::new("GET", "/ms/same", None, vec![], Bytes::new());
    let c = mockserver::convert(&json!([
        {"httpRequest": {"path": "/ms/same"}, "httpResponse": {"body": "first"}},
        {"httpRequest": {"path": "/ms/same"}, "httpResponse": {"body": "second"}}]));
    assert_eq!(
        first_match(&c.stubs, &req)
            .unwrap()
            .response
            .body
            .as_deref(),
        Some("first")
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    let later = mockserver::convert(
        &json!({"httpRequest": {"path": "/ms/same"}, "httpResponse": {"body": "later"}}),
    );
    let all: Vec<Stub> = later.stubs.iter().chain(c.stubs.iter()).cloned().collect();
    assert_eq!(
        first_match(&all, &req).unwrap().response.body.as_deref(),
        Some("first")
    );

    let native: Stub = serde_json::from_value(
        json!({"request": {"urlPath": "/ms/same"}, "response": {"body": "native"}}),
    )
    .unwrap();
    let all: Vec<Stub> = vec![native.clone()];
    assert_eq!(
        first_match(&all, &req).unwrap().response.body.as_deref(),
        Some("native")
    );
    assert!(native.order.is_none());
}

#[test]
fn d11_d12_mockserver_ttl_and_xml_order() {
    let c = mockserver::convert(
        &json!({"httpRequest": {"path": "/ms/ttl"}, "httpResponse": {},
        "timeToLive": {"timeUnit": "SECONDS", "timeToLive": 3}}),
    );
    assert_eq!(c.stubs[0].ttl_seconds, Some(3));
    let c = mockserver::convert(
        &json!({"httpRequest": {"path": "/ms/ttl"}, "httpResponse": {},
        "timeToLive": {"timeUnit": "SECONDS", "timeToLive": 3, "unlimited": true}}),
    );
    assert_eq!(c.stubs[0].ttl_seconds, None);

    let c = mockserver::convert(
        &json!({"httpRequest": {"path": "/x", "body": {"type": "XML", "xml": "<order><id>1</id><item>A</item></order>"}},
        "httpResponse": {}}),
    );
    let req = CapturedRequest::new(
        "POST",
        "/x",
        None,
        vec![],
        Bytes::from("<order><item>A</item><id>1</id></order>"),
    );
    assert!(hit(&c.stubs[0], &req));
    let req = CapturedRequest::new(
        "POST",
        "/x",
        None,
        vec![],
        Bytes::from("<order><item>B</item><id>1</id></order>"),
    );
    assert!(!hit(&c.stubs[0], &req));
}

#[test]
fn d14_several_set_cookie_headers() {
    let (s, _) = wm_one(
        json!({"request": {"urlPath": "/c"}, "response": {"headers": {"Set-Cookie": ["a=1; Expires=Wed, 21 Oct 2027 07:28:00 GMT", "b=2"]}}}),
    );
    let s = s.unwrap();
    assert_eq!(
        s.response.headers["Set-Cookie"],
        "a=1; Expires=Wed, 21 Oct 2027 07:28:00 GMT\nb=2"
    );
    let v = serde_json::to_value(&s.response).unwrap();
    assert_eq!(
        v["headers"]["Set-Cookie"],
        json!(["a=1; Expires=Wed, 21 Oct 2027 07:28:00 GMT", "b=2"])
    );
    let back: fastmock_core::ResponseDef = serde_json::from_value(v).unwrap();
    assert_eq!(back, s.response);
    assert!(
        serde_json::from_value::<fastmock_core::ResponseDef>(json!({"headers": {"x": []}}))
            .is_err()
    );

    let c = mockserver::convert(&json!({"httpRequest": {"path": "/c"}, "httpResponse": {
        "headers": {"Set-Cookie": ["a=1", "b=2"]}, "cookies": {"c": "3"}}}));
    assert_eq!(
        c.stubs[0].response.headers.len(),
        1,
        "{:?}",
        c.stubs[0].response.headers
    );
    assert_eq!(c.stubs[0].response.headers["Set-Cookie"], "a=1\nb=2\nc=3");
}

#[test]
fn d16_jayway_json_path_in_wiremock_imports() {
    let doc = json!({"mappings": [
        {"name": "regex", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$.things[?(@.name =~ /Req.*/)]"}]}},
        {"name": "in", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$.items[?(@.status in [\"A\",\"B\"])]"}]}},
        {"name": "length", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": {"expression": "$.items.length()", "equalTo": "2"}}]}},
        {"name": "size", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$[?(@.items.size() == 2)]"}]}},
        {"name": "object-filter", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$[?(@.qty > 1)]"}]}},
        {"name": "nested-object-filter", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$.order[?(@.qty > 1)]"}]}},
        {"name": "unsupported", "request": {"urlPath": "/j", "bodyPatterns": [{"matchesJsonPath": "$.tags[?(@ subsetof ['a'])]"}]}}
    ]});
    let c = wiremock::convert(&doc);
    assert_eq!(c.stubs.len(), 6, "{:?}", c.warnings);
    assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
    assert!(
        c.warnings[0].starts_with("unsupported:"),
        "the warning names the mapping: {:?}",
        c.warnings
    );
    let by = |n: &str| {
        c.stubs
            .iter()
            .find(|s| s.name.as_deref() == Some(n))
            .unwrap()
    };
    assert!(hit(
        by("regex"),
        &post("/j", &json!({"things": [{"name": "Required"}]}))
    ));
    assert!(!hit(
        by("regex"),
        &post("/j", &json!({"things": [{"name": "Other"}]}))
    ));
    assert!(hit(
        by("in"),
        &post("/j", &json!({"items": [{"status": "B"}]}))
    ));
    assert!(!hit(
        by("in"),
        &post("/j", &json!({"items": [{"status": "C"}]}))
    ));
    assert!(hit(by("length"), &post("/j", &json!({"items": [1, 2]}))));
    assert!(!hit(
        by("length"),
        &post("/j", &json!({"items": [1, 2, 3]}))
    ));
    assert!(hit(by("size"), &post("/j", &json!({"items": [1, 2]}))));
    assert!(hit(by("object-filter"), &post("/j", &json!({"qty": 2}))));
    assert!(!hit(by("object-filter"), &post("/j", &json!({"qty": 1}))));
    assert!(hit(
        by("nested-object-filter"),
        &post("/j", &json!({"order": {"qty": 2}}))
    ));
}

#[test]
fn d17_mockserver_matching_key() {
    let c = mockserver::convert(
        &json!({"httpRequest": {"path": "/k", "queryStringParameters": {"keyMatchStyle": "MATCHING_KEY", "id": ["[0-9]"]}},
        "httpResponse": {}}),
    );
    assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    let s = &c.stubs[0];
    let q = |q: &str| CapturedRequest::new("GET", "/k", Some(q), vec![], Bytes::new());
    assert!(!hit(s, &q("id=1&id=x")));
    assert!(hit(s, &q("id=1&id=2")));
    assert!(!hit(s, &q("other=1")));
    let c = mockserver::convert(
        &json!({"httpRequest": {"path": "/k", "queryStringParameters": {"id": ["[0-9]"]}}, "httpResponse": {}}),
    );
    assert!(
        hit(&c.stubs[0], &q("id=1&id=x")),
        "SUB_SET stays the default"
    );
}

#[test]
fn d18_wiremock_now_and_random_int() {
    let t = wiremock::convert_templates(
        "{{now}} {{now offset='1 days'}} {{now format='yyyy'}} {{randomInt lower=1 upper=2}}",
    );
    assert_eq!(
        t,
        "{{now format='isoSeconds'}} {{now offset='1 days' format='isoSeconds'}} {{now format='yyyy'}} {{randomInt lower=1 upper=1}}"
    );
    let req = CapturedRequest::new("GET", "/", None, vec![], Bytes::new());
    let now = render(&wiremock::convert_templates("{{now}}"), &req, &[]);
    let re = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$").unwrap();
    assert!(re.is_match(&now), "{now}");
    let int = wiremock::convert_templates("{{randomInt lower=1 upper=2}}");
    for _ in 0..30 {
        assert_eq!(render(&int, &req, &[]), "1");
    }
    assert_eq!(
        wiremock::convert_templates("{{randomInt upper=10}}"),
        "{{randomInt upper=9 lower=-2147483648}}"
    );
    let native = render("{{randomInt lower=1 upper=1}}", &req, &[]);
    assert_eq!(
        native, "1",
        "native randomInt keeps an inclusive upper bound"
    );
}

#[test]
fn d15_microcks_path_index_with_path_prefix() {
    let spec = json!({
        "openapi": "3.0.3", "info": {"title": "API Pastry - 2.0", "version": "2.0.0"},
        "paths": {"/pastry/{name}": {"get": {
            "parameters": [{"name": "name", "in": "path", "required": true, "schema": {"type": "string"}}],
            "responses": {"404": {"description": "x", "content": {"application/json": {"examples": {
                "Unknown": {"value": {"error": "pastry {{ request.path[1] }} not found"}}}}}}}}}}
    });
    let render_for = |prefix: &str, path: &str| {
        let r = fastmock_core::openapi::import(
            &spec,
            &fastmock_core::openapi::ImportOptions {
                tag: "pastry".into(),
                validate_requests: false,
                path_prefix: prefix.into(),
            },
        )
        .unwrap();
        let s = r.stubs.iter().find(|s| s.response.status == 404).unwrap();
        let req = CapturedRequest::new("GET", path, None, vec![], Bytes::new());
        let body = match &s.response.json_body {
            Some(j) => j.to_string(),
            None => s.response.body.clone().unwrap_or_default(),
        };
        Templates::new()
            .render(&body, &req.template_context(&Default::default(), false))
            .unwrap()
    };
    assert!(render_for("", "/pastry/Croissant").contains("pastry Croissant not found"));
    assert!(
        render_for(
            "/rest/API+Pastry+-+2.0/2.0.0",
            "/rest/API+Pastry+-+2.0/2.0.0/pastry/Croissant"
        )
        .contains("pastry Croissant not found")
    );
}
