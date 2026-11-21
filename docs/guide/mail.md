# Mail

FastMock captures the email an application sends and gives tests a way to wait for it, read it and check it. It
also feeds email to applications that read a mailbox. Applications send over SMTP or through the HTTP APIs of
Amazon SES, SendGrid, Mailgun, Postmark, Resend and Brevo. FastMock stores each message in a space, and tests
read it through the admin API, IMAP, POP3 or the web UI. FastMock can also send the delivery and inbound webhooks
those providers send.

**FastMock never delivers mail anywhere.** It has no relay, forwarding or outgoing SMTP. A message sent to
FastMock is stored, and that is the end of its journey. Point applications at it only in test environments.

- [Enabling mail](#enabling-mail)
- [Sending a first email](#sending-a-first-email)
- [Reading mail through the admin API](#reading-mail-through-the-admin-api)
- [Waiting and verifying in tests](#waiting-and-verifying-in-tests)
- [Labels](#labels)
- [Spaces](#spaces)
- [Chaos for mail](#chaos-for-mail)
- [SMTP details](#smtp-details)
- [IMAP and POP3](#imap-and-pop3)
- [Injecting messages](#injecting-messages)
- [Provider API mocks](#provider-api-mocks)
- [Provider webhooks](#provider-webhooks)
- [Mail in the web UI](#mail-in-the-web-ui)
- [Limits and retention](#limits-and-retention)
- [Metrics](#metrics)
- [Troubleshooting](#troubleshooting)

## Enabling mail

Mail is off by default. `FASTMOCK_MAIL_ENABLED=true` turns it on. SMTP then listens on port 2525; IMAP, POP3
and the implicit-TLS ports are off until you give them an address.

| Variable | Default | Effect |
|---|---|---|
| `FASTMOCK_MAIL_ENABLED` | `false` | Turns the mail module on. While it is off, every other `FASTMOCK_MAIL_*` variable is ignored, `/__admin/mail/*` answers `404` and `/__mail/*` is an ordinary data-plane path. |
| `FASTMOCK_MAIL_SMTP_LISTEN` | `0.0.0.0:2525` | SMTP with optional STARTTLS. An empty value turns it off. |
| `FASTMOCK_MAIL_SMTPS_LISTEN` | off | SMTP over implicit TLS, for example `0.0.0.0:4650`. Needs a certificate. |
| `FASTMOCK_MAIL_IMAP_LISTEN`, `FASTMOCK_MAIL_IMAPS_LISTEN` | off | IMAP (STARTTLS), IMAP over implicit TLS, for example `0.0.0.0:1143` and `0.0.0.0:1993`. |
| `FASTMOCK_MAIL_POP3_LISTEN`, `FASTMOCK_MAIL_POP3S_LISTEN` | off | POP3 (STLS), POP3 over implicit TLS, for example `0.0.0.0:1110` and `0.0.0.0:1995`. |
| `FASTMOCK_MAIL_TLS_CERT_FILE`, `FASTMOCK_MAIL_TLS_KEY_FILE` | the data-plane certificate | Certificate and key for STARTTLS, STLS and the implicit-TLS ports. |

Mail ports must differ from each other and from `FASTMOCK_LISTEN` and `FASTMOCK_ADMIN_LISTEN`; the server refuses
to start otherwise, with a message such as
`FASTMOCK_MAIL_SMTP_LISTEN (0.0.0.0:8080) and FASTMOCK_LISTEN (0.0.0.0:8080) use the same port; mail listeners need ports of their own`.
Every setting, including limits and retention, is listed in [Configuration](../operations/configuration.md#mail).

With Docker, publish the mail ports next to the HTTP ports (see [Getting started](getting-started.md) for the
token):

```sh
docker run --rm --name fastmock \
  -p 127.0.0.1:8080:8080 -p 127.0.0.1:9090:9090 \
  -p 127.0.0.1:2525:2525 -p 127.0.0.1:1143:1143 \
  -e FASTMOCK_STORAGE=memory \
  -e FASTMOCK_ADMIN_TOKENS="me:admin:$FASTMOCK_TOKEN_HASH" \
  -e FASTMOCK_MAIL_ENABLED=true \
  -e FASTMOCK_MAIL_IMAP_LISTEN=0.0.0.0:1143 \
  fastmock:0.1.0
```

The start-up log has one `mail listener started` line per listener. `GET /__admin/version` lists the mail
features: `mail` and `mail.providers` whenever mail is on, `mail.imap` and `mail.pop3` when one of their listeners
is on.

```sh
curl -s localhost:9090/__admin/version -H "Authorization: Bearer $FASTMOCK_TOKEN" | jq -c .features
```

```json
["grpc","kafka","sse","chaos","recording","verification","files","import-openapi","import-wiremock","import-postman","import-har","mail","mail.imap","mail.providers"]
```

With PostgreSQL storage, messages are stored in PostgreSQL and new-message events travel through Valkey, so any
replica can accept a message and any replica can answer a test that waits for it
([Database](../operations/database.md#what-is-stored-where)). In memory mode they stay in the process.

## Sending a first email

Point the application's SMTP settings at host `localhost` (or the FastMock Service), port `2525`, without TLS and
with any user name and password, or none. Without a certificate FastMock offers neither STARTTLS nor TLS, and
accepts every message.

=== "swaks"

    ```sh
    swaks --server localhost:2525 \
      --from noreply@example.com --to alice@example.test \
      --header "Subject: Verify your address" \
      --body "Your code is 482913"
    ```

=== "curl"

    ```sh
    printf 'From: noreply@example.com\r\nTo: alice@example.test\r\nSubject: Verify your address\r\n\r\nYour code is 482913\r\n' > message.eml
    curl smtp://localhost:2525 \
      --mail-from noreply@example.com --mail-rcpt alice@example.test \
      --upload-file message.eml
    ```

=== "Python"

    ```python
    import smtplib
    from email.message import EmailMessage

    msg = EmailMessage()
    msg["From"] = "Example Shop <noreply@example.com>"
    msg["To"] = "alice@example.test"
    msg["Subject"] = "Verify your address"
    msg.set_content("Your code is 482913")
    msg.add_alternative("<p>Your code is <b>482913</b></p>", subtype="html")

    with smtplib.SMTP("localhost", 2525) as smtp:
        smtp.send_message(msg)
    ```

=== "Node (nodemailer)"

    ```js
    import nodemailer from "nodemailer";

    const transport = nodemailer.createTransport({ host: "localhost", port: 2525, secure: false });
    await transport.sendMail({
      from: "Example Shop <noreply@example.com>",
      to: "alice@example.test",
      subject: "Verify your address",
      text: "Your code is 482913",
      html: "<p>Your code is <b>482913</b></p>",
    });
    ```

=== "Java (Jakarta Mail)"

    ```java
    import jakarta.mail.*;
    import jakarta.mail.internet.*;
    import java.util.Properties;

    var props = new Properties();
    props.put("mail.smtp.host", "localhost");
    props.put("mail.smtp.port", "2525");
    var session = Session.getInstance(props);

    var msg = new MimeMessage(session);
    msg.setFrom(new InternetAddress("noreply@example.com", "Example Shop"));
    msg.setRecipients(Message.RecipientType.TO, InternetAddress.parse("alice@example.test"));
    msg.setSubject("Verify your address");
    msg.setText("Your code is 482913");
    Transport.send(msg);
    ```

The SMTP reply to the message carries its id: `250 2.0.0 OK: queued as 019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e`.
Spring Boot applications set `spring.mail.host` and `spring.mail.port`; most frameworks have the same two
settings.

## Reading mail through the admin API

Every mail route is under `/__admin/mail` on the admin port, takes `?space=` like the other admin routes and
needs a [`mail:*` permission](#spaces). While mail is off they answer
`404 {"error":"mail is not enabled; set FASTMOCK_MAIL_ENABLED=true",...}`.

### Listing and searching

```sh
curl -s "localhost:9090/__admin/mail/messages?query=to:alice%20subject:verify&limit=10" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{
  "messages": [
    {
      "id": "019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e",
      "receivedAt": "2026-11-21T09:14:03.512408Z",
      "source": "smtp",
      "envelope": {"mailFrom": "noreply@example.com", "rcptTo": ["alice@example.test"],
                   "helo": "app.example.com", "clientIp": "198.51.100.7", "tls": false},
      "summary": {"subject": "Verify your address",
                  "from": [{"name": "Example Shop", "address": "noreply@example.com"}],
                  "to": [{"address": "alice@example.test"}], "cc": [],
                  "messageId": "3f1c9a@app.example.com", "size": 912, "hasAttachments": false,
                  "hasText": true, "hasHtml": true, "preview": "Your code is 482913"},
      "labels": []
    }
  ],
  "nextCursor": "MjAyNi0xMS0yMVQwOToxNDowMy41MTI0MDha..."
}
```

Lists hold summaries only. Parameters: `query` (below), `limit` (1 to 500, default 50), `cursor` (the
`nextCursor` of the previous page, absent on the last page) and `order` (`newest`, the default, or `oldest`).
Without `space`, the list covers every area the caller may read; the global area only for callers that hold
`mail:read` globally.

The `query` syntax is shared with the search box of the web UI. All terms must hold; values with spaces go in
double quotes; matching ignores case.

| Term | Matches messages |
|---|---|
| `from:billing` | whose `From` address or name, or envelope sender, contains `billing` |
| `to:alice` | whose `To` address or name, or an envelope recipient (so Bcc recipients too), contains `alice` |
| `cc:alice` | whose `Cc` address or name contains `alice` |
| `subject:"monthly invoice"` | whose subject contains `monthly invoice` |
| `has:attachment` | with at least one attachment |
| `label:otp` | with the label `otp` (exact) |
| `after:2026-11-21` | received at or after this moment (a date means midnight UTC; an RFC 3339 time also works) |
| `before:2026-11-21T12:00:00Z` | received before this moment |
| any other word, or `key:value` with another key | whose subject or text contains it (so `https://app.example.com/verify` can be searched for) |

A query is at most 1024 bytes and 32 terms. Errors answer `400`, for example
`has:pdf is not supported; use has:attachment` or
`after:yesterday is not a date: use YYYY-MM-DD or an RFC 3339 time`.

### One message and its renditions

`GET /__admin/mail/messages/{id}` returns the stored entry plus the parsed message: headers in order (decoded and
unfolded), `from`, `to`, `cc`, `bcc`, `replyTo`, `subject`, `date`, `messageId`, `text`, `hasHtml`, `attachments`
(each with `partId`, `name`, `contentType`, `size`, `contentId`, `inline`), labels, the provider fields of
provider messages (`providerExtras`: `providerMessageId`, `templateId`, `templateData`, `tags`, `metadata`,
`testMode`) and `urls` with the paths below. The HTML body is never inline in JSON.

| Path below `/__admin/mail/messages/{id}` | Returns |
|---|---|
| `/raw` | The message exactly as received, `message/rfc822`, as a download (`<id>.eml`). |
| `/html` | The HTML body, with `cid:` images of the message turned into `data:` URIs (4 MiB in total). Served with `Content-Security-Policy: sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'`, so no script runs and nothing is loaded. `?images=allow` also allows remote `https:` images. `404` when there is no HTML body. |
| `/text` | The text body (`text/plain; charset=utf-8`); for HTML-only messages, the HTML converted to text. |
| `/parts/{partId}` | An attachment or inline part as a download, with a sanitised file name. |
| `/transcript` | `{transcript: [{at, direction, text}]}`: the SMTP session that delivered the message, credentials masked and the message body replaced by its size; empty for messages that did not arrive over SMTP. |
| `/links` | `{links: [{url, text}]}`: every distinct `http(s)` URL of the text and HTML bodies, with the anchor text. |
| `/codes?pattern=` | `{codes, pattern}`: distinct codes in the subject and text, in order. The default pattern is `\b\d{4,8}\b`; with a capture group, group 1 is the code. Patterns are at most 256 bytes. |

```sh
curl -s localhost:9090/__admin/mail/messages/019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e/codes \
  -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{"codes": ["482913"], "pattern": "\\b\\d{4,8}\\b"}
```

```sh
curl -s -G localhost:9090/__admin/mail/messages/019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e/codes \
  --data-urlencode 'pattern=code: ([A-Z0-9]{6})' -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```sh
curl -s localhost:9090/__admin/mail/messages/019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e/links \
  -H "Authorization: Bearer $FASTMOCK_TOKEN"
```

```json
{"links": [{"url": "https://app.example.com/verify?token=8f2c41", "text": "Verify your address"}]}
```

FastMock extracts links but never follows them. To open the HTML yourself, use `/html` in an `<iframe sandbox>`
without `allow-scripts` and `allow-same-origin`.

### Deleting

| Request | Effect |
|---|---|
| `DELETE /__admin/mail/messages/{id}` | Deletes one message (`204`). |
| `DELETE /__admin/mail/messages?query=...` | Deletes the matching messages of the space and answers `{deleted, ids}`. Without `query`, every message of the space. |

Deleting through the admin API removes the message from every IMAP and POP3 mailbox too. Deletes are audit
events (`mail.message.delete`, `mail.messages.delete`).

## Waiting and verifying in tests

### Waiting for a message

`POST /__admin/mail/messages/wait` blocks until enough matching messages have arrived, up to 60 seconds:

```sh
curl -s localhost:9090/__admin/mail/messages/wait -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"match": {"to": {"equalTo": "alice@example.test"}, "subject": {"contains": "Verify"}},
       "count": 1, "timeoutMs": 10000}'
```

| Field | Default | Meaning |
|---|---|---|
| `match` | any message | A [`MailMatch`](#matching-messages). |
| `count` | `1` | Messages to wait for, 1 to 500. |
| `timeoutMs` | `10000` | How long to wait, 0 to 60000. |
| `includeExisting` | `false` | Also count matching messages already stored, whenever they arrived. |
| `since` | none | Also count stored messages received at or after this time (RFC 3339). |
| `lookbackMs` | `0` | Without `since`: also count stored messages received up to this many milliseconds before the request (at most 7 days). |

By default only messages that arrive after the request count. A test that triggers the email first and waits
afterwards should take a timestamp before the action and pass it as `since`, or send to a unique address and set
`includeExisting: true`.

`200` returns the first `count` matching messages, oldest first:

```json
{"messages": [{"id": "019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e", "receivedAt": "2026-11-21T09:14:03.512408Z", "source": "smtp", "...": "..."}], "count": 1}
```

`408` reports what arrived in time:

```json
{"error": "timed out after 10000 ms: 0 of 1 matching messages received", "details": [], "count": 0, "expected": 1, "messages": []}
```

The wait works on any replica. It stops when the client disconnects, and answers
`503 the server is shutting down; retry on another replica` while a replica shuts down. A wait that would scan
more than 10,000 stored messages answers `422`; narrow it with `space` or `since`.

### Verifying a count

`POST /__admin/mail/verify` checks stored messages without waiting, in the style of
[`/verify`](verification.md) for HTTP requests:

```sh
curl -s localhost:9090/__admin/mail/verify -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' \
  -d '{"match": {"to": {"equalTo": "alice@example.test"}, "subject": {"contains": "Welcome"}}, "count": 1}'
```

Give `count` for an exact number, or `atLeast` and/or `atMost`; without any of them the default is at least 1.
`since` limits the check to messages received at or after a time. `200 {"verified": true, ...}` when it holds;
otherwise `422` with the closest messages and the matchers they failed:

```json
{
  "verified": false,
  "count": 0,
  "expected": "exactly 1",
  "error": "expected exactly 1 matching messages, received 0",
  "details": ["message 019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e (Verify your address): subject"],
  "nearMisses": [
    {"id": "019a9c4e-5b1f-7c2a-9d3e-4f5a6b7c8d9e", "receivedAt": "2026-11-21T09:14:03.512408Z",
     "subject": "Verify your address", "from": [{"address": "noreply@example.com"}],
     "to": ["alice@example.test"], "score": 0.5, "mismatches": ["subject"]}
  ]
}
```

`400` for `use either count or atLeast/atMost`, `atLeast must not exceed atMost` and `invalid match: ...`.

### Matching messages

A `MailMatch` is an object of optional conditions; all given ones must hold. Each condition is a
[string matcher](request-matching.md) as in stubs: `equalTo`, `contains`, `matches`, `caseInsensitive`,
`and`/`or`/`not`, `absent` and the others.

| Field | Matches |
|---|---|
| `from` | a `From` address (`noreply@example.com` or `Example Shop <noreply@example.com>`) |
| `to` | a `To` address or an envelope recipient, so Bcc recipients are found |
| `cc` | a `Cc` address |
| `subject` | the subject |
| `text` | the text body (the HTML converted to text when there is no text part) |
| `html` | the HTML body as written |
| `headers` | a map of header name (case-insensitive) to matcher; a header present several times matches when one occurrence matches |
| `hasAttachment` | `true` or `false` |
| `attachmentName` | the file name of one attachment |
| `label` | one label |
| `source` | `smtp`, `provider` or `inject` |
| `provider` | `ses`, `sendgrid`, `mailgun`, `postmark`, `resend` or `brevo` |

```json
{
  "match": {
    "from": {"contains": "@example.com"},
    "to": {"equalTo": "alice@example.test"},
    "headers": {"X-Template": {"equalTo": "verify-email"}},
    "hasAttachment": false
  }
}
```

### In test code

=== "Java"

    The [Java client](java-client.md) has no mail methods yet; the JDK HTTP client is enough:

    ```java
    import java.net.URI;
    import java.net.http.*;
    import java.time.Duration;
    import java.time.Instant;

    var http = HttpClient.newHttpClient();
    var admin = "http://localhost:9090";
    var token = System.getenv("FASTMOCK_TOKEN");
    var since = Instant.now();

    signUp("alice@example.test");

    var body = """
        {"match": {"to": {"equalTo": "alice@example.test"}, "subject": {"contains": "Verify"}},
         "since": "%s", "timeoutMs": 15000}
        """.formatted(since);
    var request = HttpRequest.newBuilder(URI.create(admin + "/__admin/mail/messages/wait"))
        .header("Authorization", "Bearer " + token)
        .header("Content-Type", "application/json")
        .timeout(Duration.ofSeconds(20))
        .POST(HttpRequest.BodyPublishers.ofString(body))
        .build();
    var response = http.send(request, HttpResponse.BodyHandlers.ofString());
    assertEquals(200, response.statusCode(), response.body());
    String id = objectMapper.readTree(response.body()).at("/messages/0/id").asText();

    var codes = http.send(HttpRequest.newBuilder(URI.create(admin + "/__admin/mail/messages/" + id + "/codes"))
            .header("Authorization", "Bearer " + token).build(),
        HttpResponse.BodyHandlers.ofString());
    String code = objectMapper.readTree(codes.body()).at("/codes/0").asText();
    ```

=== "Python"

    ```python
    import os
    from datetime import datetime, timezone

    import requests

    ADMIN = "http://localhost:9090"
    AUTH = {"Authorization": f"Bearer {os.environ['FASTMOCK_TOKEN']}"}

    def test_sign_up_sends_a_code():
        since = datetime.now(timezone.utc).isoformat()
        sign_up("alice@example.test")

        r = requests.post(f"{ADMIN}/__admin/mail/messages/wait", headers=AUTH, timeout=20, json={
            "match": {"to": {"equalTo": "alice@example.test"}, "subject": {"contains": "Verify"}},
            "since": since,
            "timeoutMs": 15000,
        })
        assert r.status_code == 200, r.text
        message = r.json()["messages"][0]

        codes = requests.get(f"{ADMIN}/__admin/mail/messages/{message['id']}/codes", headers=AUTH).json()
        assert len(codes["codes"]) == 1

        r = requests.post(f"{ADMIN}/__admin/mail/verify", headers=AUTH, json={
            "match": {"to": {"equalTo": "alice@example.test"}}, "count": 1, "since": since,
        })
        assert r.status_code == 200, r.json()["details"]
    ```

=== "Node (Playwright)"

    ```ts
    import { test, expect } from "@playwright/test";

    const admin = process.env.FASTMOCK_ADMIN_URL ?? "http://localhost:9090";
    const headers = { Authorization: `Bearer ${process.env.FASTMOCK_TOKEN}` };

    test("sign-up sends a verification code", async ({ page, request }) => {
      const email = `alice+run${Date.now()}@example.test`;
      await page.goto("https://app.example.com/signup");
      await page.getByLabel("Email").fill(email);
      await page.getByRole("button", { name: "Sign up" }).click();

      const wait = await request.post(`${admin}/__admin/mail/messages/wait`, {
        headers,
        data: { match: { to: { equalTo: email } }, includeExisting: true, timeoutMs: 15000 },
        timeout: 20000,
      });
      expect(wait.status()).toBe(200);
      const [message] = (await wait.json()).messages;

      const { codes } = await (await request.get(`${admin}/__admin/mail/messages/${message.id}/codes`, { headers })).json();
      await page.getByLabel("Code").fill(codes[0]);

      const { links } = await (await request.get(`${admin}/__admin/mail/messages/${message.id}/links`, { headers })).json();
      expect(links.map((l: { url: string }) => l.url)).toContain("https://app.example.com/help");
    });
    ```

A unique address per test (`alice+run123@example.test`) keeps parallel tests apart; the plus tag also becomes a
[label](#labels), and IMAP and POP3 file it into the `alice@example.test` mailbox.

## Labels

Labels are short tags on a stored message: 1 to 64 characters of letters, digits and `- _ . : /`, stored in lower
case, at most 32 per message. A message gets labels on arrival from:

- the tag of a plus-addressed recipient: `alice+signup@example.test` adds `signup`;
- the comma-separated values of `X-FastMock-Labels` headers: `X-FastMock-Labels: otp, welcome`.

`PUT /__admin/mail/messages/{id}/labels` with `{"labels": ["reviewed", "otp"]}` replaces them (audit event
`mail.message.labels`). Search with `label:otp`, match with `{"label": {"equalTo": "otp"}}`.

## Spaces

Mail is scoped by [space](../operations/access-control.md#spaces) like stubs and the journal. A space claims mail
domains in the access file:

```yaml
spaces:
  - name: billing
    mailDomains: [billing.example.test, "*.billing.example.test"]
  - name: crm
    mailDomains: [crm.example.test]
```

Domains are lowercase; `*.billing.example.test` covers its subdomains, not `billing.example.test` itself. A
domain belongs to one space: overlaps stop the start like overlapping path prefixes.

Where a message received over SMTP or a provider API is stored:

1. Each recipient whose domain a space claims goes to that space. A message to recipients of several spaces is
   stored once per space, and each copy's `envelope.rcptTo` lists only that space's recipients (the headers are
   unchanged).
2. The other recipients go to the space whose name equals the SMTP AUTH user name, when such a space exists, so an
   application can choose its space by logging in as `billing`.
3. Everything else goes to the global area.

A provider API call can name its space in the URL instead (`/__mail/sendgrid/@billing/...`, see
[Provider API mocks](#provider-api-mocks)), and injected messages go to the `space` of the request.

| Permission | Allows |
|---|---|
| `mail:read` | listing, searching and reading messages and their renditions, transcripts, links and codes; `wait`, `verify` and the event stream; reading mailboxes, mail chaos rules and webhook settings; IMAP and POP3 logins with a managed token |
| `mail:write` | injecting messages, labels, mailbox passwords, mail chaos rules, provider webhooks and expected API keys, inbound simulation; SMTP AUTH with a managed token when `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH=true` |
| `mail:delete` | deleting messages |

All three can be granted per space. `viewer` has `mail:read`; `editor` has all three. Messages of the global area
are visible only with `mail:read` granted globally. A message the caller cannot read answers `404 message not found`.

## Chaos for mail

Mail chaos rules make the SMTP server (and the [provider APIs](#provider-errors)) answer with errors, delays or
dropped connections, so you can test retries and error handling. Rules belong to a space or the global area:

```sh
curl -s -X PUT "localhost:9090/__admin/mail/chaos?space=billing" -H "Authorization: Bearer $FASTMOCK_TOKEN" \
  -H 'content-type: application/json' -d '{
  "rules": [
    {"id": "third-message-fails", "stage": "data", "every": 3,
     "action": {"reply": {"code": 451, "enhanced": "4.3.0", "text": "Try again later"}}},
    {"id": "slow-data", "stage": "data", "probability": 20, "seed": 7, "action": {"delay": "5s"}},
    {"id": "drop-reports", "stage": "rcpt", "match": {"rcptTo": {"contains": "reports@"}},
     "action": {"disconnect": true}, "times": 1}
  ]}'
```

```json
{"space": "billing", "rules": [{"id": "third-message-fails", "...": "..."}],
 "counters": [{"id": "third-message-fails", "matched": 0, "fired": 0}, {"id": "slow-data", "matched": 0, "fired": 0}, {"id": "drop-reports", "matched": 0, "fired": 0}]}
```

`GET` returns the rules with their counters (`mail:read`); `PUT` replaces them and `DELETE` removes them
(`mail:write`, audit events `mail.chaos.set` and `mail.chaos.clear`). Invalid rules answer `400` naming the rule,
for example `rules[0]: ...`.

| Field | Meaning |
|---|---|
| `id` | Letters, digits, `.`, `_`, `-`, at most 64; `rule-<n>` when left out. |
| `stage` | Where the rule applies (table below). |
| `match` | Optional conditions, written as string matchers: `rcptTo` (stages `rcpt`, `data`, `provider`), `mailFrom` (`mail`, `rcpt`, `data`, `provider`), `authUser` (`auth`, `mail`, `rcpt`, `data`), `provider` (`provider`). |
| `action` | Optionally `delay` (`500ms`, `2s`, at most 10 minutes), then at most one of `reply` (`code` 400 to 599, `enhanced` status code of the same class, default `<class>.0.0`, and `text`, default `FastMock chaos rule <id>`), `disconnect: true` (close without a reply) and `providerError`. A `delay` alone just slows the step down. |
| `times` | Fire at most this many times. |
| `every` | Fire only on every n-th matching event (the n-th, 2n-th, ...). |
| `probability` | Chance to fire, 0 to 100. With `seed`, the decision follows the seed and the number of the event, so a run repeats. |

| Stage | Evaluated | Rules of |
|---|---|---|
| `connect` | on a new connection; a reply replaces the greeting, then the connection closes | the global area |
| `tls` | on `STARTTLS` (the reply replaces `220 Ready to start TLS`); on SMTPS before the handshake | the global area |
| `ehlo` | on `EHLO`/`HELO` | the global area |
| `auth` | after `AUTH` credentials were read | the space named like the AUTH user, else global |
| `mail` | on `MAIL FROM` | the space named like the AUTH user, else global |
| `rcpt` | on each `RCPT TO` | the space the recipient is routed to |
| `data` | after the end of the message, before it is stored | each space the message is routed to |
| `provider` | on a provider API call | the space of the call |

Rules are evaluated in order; the first rule that fires wins. A rule that matches but does not fire (because of
`every`, `probability` or `times`) passes to the next one. Counters are shared through Valkey, so "every third
message" counts across all replicas, and they restart when the rules are replaced. A change reaches every replica
within 2 seconds. A reply with code `421` also closes the connection. At most 100 rules per area.

Without any rule, a recipient of the form `fail+<code>@<any domain>` is refused at `RCPT` with that code (400 to
599): `RCPT TO:<fail+550@example.test>` gets `550 5.0.0 Rejected by the fail+550 address`.

More examples:

```json
{"rules": [
  {"stage": "connect", "times": 1, "action": {"reply": {"code": 421, "text": "Service not available"}}},
  {"stage": "mail", "match": {"mailFrom": {"equalTo": "alerts@example.com"}}, "action": {"reply": {"code": 550, "enhanced": "5.7.1", "text": "Sender rejected"}}},
  {"stage": "data", "action": {"delay": "30s", "disconnect": true}}
]}
```

Chaos firings are counted in `fastmock_mail_chaos_injections_total{stage}`.

## SMTP details

### Capabilities

The greeting is `220 <FASTMOCK_MAIL_HOSTNAME> ESMTP FastMock` (`localhost` by default). `EHLO` answers:

```text
250-localhost greets app.example.com
250-SIZE 10485760
250-8BITMIME
250-SMTPUTF8
250-PIPELINING
250-CHUNKING
250-ENHANCEDSTATUSCODES
250-STARTTLS
250 AUTH PLAIN LOGIN CRAM-MD5
```

`STARTTLS` appears only when a certificate is configured and TLS is not active yet. `AUTH` appears once TLS is
active when a certificate is configured (before that only with `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH=true`), and
always when there is no certificate. Commands: `EHLO`, `HELO`, `MAIL`, `RCPT`, `DATA`, `BDAT`, `RSET`, `NOOP`,
`VRFY` (always `252`), `QUIT`, `STARTTLS`, `AUTH`. `EXPN`, `HELP`, `ETRN`, `ATRN`, `BURL` and `LHLO` answer
`502`. UTF-8 addresses need `MAIL FROM:<...> SMTPUTF8`; `BODY=BINARYMIME` is not supported.

### TLS

| Setting | Behaviour |
|---|---|
| `FASTMOCK_MAIL_TLS_CERT_FILE` and `FASTMOCK_MAIL_TLS_KEY_FILE` | PEM certificate chain and key for STARTTLS (SMTP, IMAP), STLS (POP3) and the implicit-TLS listeners. Without them, the data-plane certificate (`FASTMOCK_TLS_CERT_FILE`, `FASTMOCK_TLS_KEY_FILE`) is used when set. Setting only one of the two is a start-up error. |
| `FASTMOCK_MAIL_SMTPS_LISTEN` | Implicit TLS from the first byte (port 465 style). Refused at start-up without a certificate. |
| no certificate | STARTTLS is not offered (`502 5.5.1 STARTTLS is not offered`), AUTH works over plain TCP. |

After `STARTTLS` the session starts over: the client sends `EHLO` again. A second `STARTTLS` answers
`503 5.5.1 TLS is already active`. A client that sends further commands in the same packet as `STARTTLS` gets the
`220` and the connection is closed. TLS 1.2 and 1.3 only.

### Authentication

By default FastMock accepts any credentials with `AUTH PLAIN`, `LOGIN` or `CRAM-MD5` (any digest), and messages
without AUTH. The user name is recorded in `envelope.authUser` and used for [routing](#spaces); passwords are never
stored or logged, and the transcript masks the credentials.

To require authentication, set `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH=true`. `MAIL` without AUTH then answers
`530 5.7.0 Authentication required`. Two kinds of credentials work:

- users of `FASTMOCK_MAIL_SMTP_USERS`: `user:sha256hex` entries, separated by commas or new lines, where the hash
  is the hex SHA-256 of the password, as for admin tokens:

  ```sh
  export FASTMOCK_MAIL_SMTP_USERS="billing-app:$(fastmock hash-token 'a-long-random-password')"
  ```

- a [managed token](../operations/access-control.md#managed-tokens) as the password, with `mail:write` granted
  globally or in the space named like the user name.

With required authentication, `CRAM-MD5` is not offered (`AUTH PLAIN LOGIN`). Wrong credentials answer
`535 5.7.8 Authentication credentials invalid`; after `FASTMOCK_MAIL_MAX_AUTH_FAILURES` (3) failures the
connection is closed with `421 4.7.0 Too many failed authentication attempts; closing connection`.

### Limits

| Limit | Default | Setting | Reply |
|---|---|---|---|
| message size (advertised as `SIZE`) | 10 MiB | `FASTMOCK_MAIL_MAX_MESSAGE_BYTES` | `552 5.3.4 Message size exceeds fixed maximum message size` |
| recipients per message | 100 | `FASTMOCK_MAIL_MAX_RECIPIENTS` | `452 4.5.3 Too many recipients` |
| command line | 4096 bytes | `FASTMOCK_MAIL_MAX_LINE_BYTES` | `500 5.5.2 Line too long` |
| wait for the next command | 30 s | `FASTMOCK_MAIL_IDLE_TIMEOUT` | `421 4.4.2 Timeout waiting for the client; closing connection` |
| wait for the next DATA or BDAT chunk | 60 s | `FASTMOCK_MAIL_DATA_TIMEOUT` | same |
| session length | 10 min | `FASTMOCK_MAIL_SESSION_TIMEOUT` | `421 4.4.2 Session time limit reached; closing connection` |
| connections over all mail listeners / per client IP | 256 / 32 | `FASTMOCK_MAIL_MAX_CONNECTIONS`, `FASTMOCK_MAIL_MAX_CONNECTIONS_PER_IP` | `421 4.7.0 Too many connections, try again later` (SMTPS: closed) |
| failed AUTH attempts | 3 | `FASTMOCK_MAIL_MAX_AUTH_FAILURES` | `421 4.7.0` and close |
| commands per session / refused commands in a row | 10,000 / 20 | fixed | `421 4.7.0 Too many errors; closing connection` |

The message ends only at `<CRLF>.<CRLF>`; a bare line feed or carriage return never ends it.

### Replies

| Situation | Reply |
|---|---|
| greeting | `220 <host> ESMTP FastMock` |
| `HELO` | `250 <host>` |
| `MAIL`, `RCPT` accepted | `250 2.1.0 Sender OK`, `250 2.1.5 Recipient OK` |
| message stored | `250 2.0.0 OK: queued as <id>` |
| `BDAT` chunk that is not the last | `250 2.0.0 <n> octets received` |
| `NOOP`, `RSET` | `250 2.0.0 OK` |
| `VRFY` | `252 2.5.0 Cannot VRFY user, but will accept the message` |
| `DATA` | `354 Start mail input; end with <CRLF>.<CRLF>` |
| `QUIT` | `221 2.0.0 Bye` |
| `STARTTLS` | `220 2.0.0 Ready to start TLS` |
| AUTH success | `235 2.7.0 Authentication successful` |
| AUTH before TLS when TLS is available | `538 5.7.11 Encryption required for requested authentication mechanism` |
| AUTH cancelled with `*` | `501 5.0.0 Authentication cancelled` |
| unknown AUTH mechanism | `504 5.5.4 Unrecognized authentication type` |
| AUTH out of order | `503 5.5.1 Send EHLO first`, `Already authenticated`, `AUTH is not allowed during a mail transaction` |
| commands out of order | `503 5.5.1 Send HELO or EHLO first`, `Sender already given`, `Send MAIL first` |
| `DATA` without recipients | `554 5.5.1 No valid recipients` |
| mixing `DATA` and `BDAT` | `503 5.5.1 DATA cannot be mixed with BDAT`, `RCPT is not allowed during BDAT` |
| `BODY=BINARYMIME` | `555 5.5.4 BODY=BINARYMIME is not supported` |
| UTF-8 address without `SMTPUTF8` | `553 5.6.7 Non-ASCII address needs SMTPUTF8` |
| invalid sender or recipient | `501 5.1.7`, `501 5.1.3` |
| syntax error | `501 5.5.4 Syntax: ...` |
| unknown command | `500 5.5.1 Command not recognized` |
| unsupported command | `502 5.5.1 <command> is not implemented` |
| `fail+<code>@` recipient | `<code> <class>.0.0 Rejected by the fail+<code> address` |
| storing failed | `451 4.3.0 Temporary failure storing the message` |
| shutdown | `421 4.3.2 Service shutting down; closing connection` |

## IMAP and POP3

Applications that read a mailbox (a support inbox, a reply parser, a bounce processor) can log in to FastMock
over IMAP or POP3. Turn the listeners on with `FASTMOCK_MAIL_IMAP_LISTEN` and `FASTMOCK_MAIL_POP3_LISTEN` (and
their implicit-TLS twins).

### Mailboxes

Every recipient address has a mailbox, created with its first message. The mailbox name is the address in lower
case without its plus tag, so `Alice+news@example.test` and `alice@example.test` share one. A mailbox has an
`INBOX`; IMAP clients may create more folders (at most 100). Messages land in the `INBOX` of each recipient.

A mailbox lives in the space that claims its domain in `mailDomains`, otherwise in the global area. Inject
messages and set passwords in that area, so that logins find them. `GET /__admin/mail/mailboxes` lists the
mailboxes with their folders and whether a password is set.

Deleting or moving a message over IMAP or POP3 changes only that mailbox; the message stays in the admin API and
in the other recipients' mailboxes. Deleting it through the admin API (or retention) removes it everywhere.

### Logging in

The user name is the mailbox address. The password is either:

- a [managed token](../operations/access-control.md#managed-tokens) holding `mail:read` in the mailbox's space or
  globally (a global-area mailbox needs a global grant), or
- a mailbox password set through the admin API (`mail:write`):

```sh
curl -s -X PUT "localhost:9090/__admin/mail/mailboxes/support@billing.example.test/password?space=billing" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' \
  -d '{"password": "support-inbox-test-password"}'
```

The password is 1 to 1024 bytes without control characters; only a salted PBKDF2 hash is stored, and the audit
event `mail.mailbox.password.set` names the address only. `DELETE` on the same path removes it. Wrong credentials
are answered after a short delay; after `FASTMOCK_MAIL_MAX_AUTH_FAILURES` (3) failures the connection closes.

When a certificate is configured, logins need TLS first (STARTTLS, STLS or the implicit-TLS port) unless
`FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH=true`: IMAP advertises `LOGINDISABLED` and answers
`NO [PRIVACYREQUIRED] ...`, POP3 answers
`-ERR [AUTH] Plaintext authentication is disabled on this connection; use STLS first`. Without a certificate,
plain logins work.

### IMAP

IMAP4rev1 with `IDLE`, `UIDPLUS`, `MOVE`, `NAMESPACE`, `ID`, `ENABLE` (`UTF8=ACCEPT`), `UNSELECT`, `SASL-IR`
and `APPENDLIMIT`; the capabilities list only what is implemented. Commands: `CAPABILITY`, `NOOP`, `LOGOUT`,
`STARTTLS`, `LOGIN`, `AUTHENTICATE PLAIN`, `ID`, `ENABLE`, `NAMESPACE`, `SELECT`, `EXAMINE`, `CREATE`, `DELETE`,
`LIST`, `LSUB`, `STATUS`, `APPEND`, `CHECK`, `CLOSE`, `UNSELECT`, `EXPUNGE`, `SEARCH`, `FETCH`, `STORE`, `COPY`,
`MOVE`, the `UID` forms, and `IDLE`. `SUBSCRIBE` and `UNSUBSCRIBE` answer `OK` without effect; `RENAME` is not
supported. `SEARCH` supports `ALL`, `SEEN`, `UNSEEN`, `FROM`, `TO`, `SUBJECT`, `SINCE`, `BEFORE`, `TEXT`,
`HEADER` and UID sets. `IDLE` is woken by new messages on any replica and ends after
`FASTMOCK_MAIL_IMAP_IDLE_TIMEOUT` (30 minutes) or with the session (`FASTMOCK_MAIL_SESSION_TIMEOUT`, 10 minutes),
whichever comes first; clients then log in again.

```python
import imaplib

with imaplib.IMAP4("localhost", 1143) as imap:
    imap.login("support@billing.example.test", "support-inbox-test-password")
    imap.select("INBOX")
    _, data = imap.search(None, "UNSEEN", "SUBJECT", "Refund")
    for num in data[0].split():
        _, parts = imap.fetch(num, "(RFC822)")
        raw = parts[0][1]
```

```sh
curl -s -u 'support@billing.example.test:support-inbox-test-password' \
  imap://localhost:1143/INBOX -X 'SEARCH ALL'
curl -s -u 'support@billing.example.test:support-inbox-test-password' \
  'imap://localhost:1143/INBOX;UID=1'
```

### POP3

Commands: `USER`, `PASS`, `AUTH PLAIN`, `CAPA`, `STLS`, `STAT`, `LIST`, `UIDL`, `RETR`, `TOP`, `DELE`, `NOOP`,
`RSET`, `QUIT` (`APOP` is refused). The maildrop is the `INBOX` as it was at login. `DELE` takes effect only at
`QUIT`; a dropped connection removes nothing. Retrieved messages that were not deleted are marked `\Seen`.

```python
import poplib

pop = poplib.POP3("localhost", 1110)
pop.user("support@billing.example.test")
pop.pass_("support-inbox-test-password")
count, size = pop.stat()
for i in range(1, count + 1):
    lines = pop.retr(i)[1]
pop.quit()
```

```sh
curl -s -u 'support@billing.example.test:support-inbox-test-password' pop3://localhost:1110/1
```

### Mail client settings

| Setting | IMAP | POP3 | SMTP |
|---|---|---|---|
| Server | FastMock host | FastMock host | FastMock host |
| Port | `1143` (STARTTLS or none), `1993` (SSL/TLS) | `1110` (STARTTLS or none), `1995` (SSL/TLS) | `2525` (STARTTLS or none), `4650` (SSL/TLS) |
| Connection security | STARTTLS when a certificate is configured, otherwise none | same | same |
| Authentication | normal password | normal password | normal password, or none |
| User name | the mailbox address | the mailbox address | anything, or a user of `FASTMOCK_MAIL_SMTP_USERS` |
| Password | mailbox password or managed token | mailbox password or managed token | anything, or as configured |

The ports are the suggested addresses above; use whatever you set in the `FASTMOCK_MAIL_*_LISTEN` variables. A
certificate that clients do not trust needs to be accepted or added to their trust store.

## Injecting messages

To test an application that reads mail, put a message straight into a space and its mailboxes without SMTP:

```sh
curl -s -X POST "localhost:9090/__admin/mail/messages?space=billing" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "from": "Alice <alice@example.com>",
  "to": ["support@billing.example.test"],
  "subject": "Refund for order 42",
  "text": "Please refund order 42.",
  "html": "<p>Please refund <b>order 42</b>.</p>",
  "headers": {"X-FastMock-Labels": "refund"},
  "attachments": [{"name": "receipt.pdf", "contentType": "application/pdf", "base64": "JVBERi0xLjQK..."}]
}'
```

`201` returns `{messages: [...]}` with the stored entry (`source: inject`). Fields: `from` and `to` (required),
`cc`, `subject`, `text`, `html`, `headers` (further header fields; `Message-ID` and `Date` replace the generated
ones; `From`, `To`, `Cc`, `Subject` and MIME structure headers cannot be set this way, and line breaks are refused),
`attachments` (`name`, `contentType`, `base64`, and `contentId` for an inline part referenced as
`cid:<contentId>`) and `mailbox` (file it into this address's `INBOX` instead of each recipient's).

A raw message works too:

```sh
curl -s -X POST "localhost:9090/__admin/mail/messages?space=billing&rcptTo=support@billing.example.test" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: message/rfc822' --data-binary @reply.eml
```

With `message/rfc822`, `mailFrom` and `rcptTo` (comma-separated) set the envelope; by default it comes from the
`From`, `To`, `Cc` and `Bcc` headers. Injected messages go to the `space` of the request (without it, the global
area, or the caller's only space), not through domain routing; IMAP and POP3 find them when that is the space
that claims the recipient's domain. Errors: `400` for invalid addresses, header fields or base64 and for
`the message names no recipients (To, Cc, Bcc); set rcptTo`, `413` above `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`, `415`
for other content types. Injection needs `mail:write` and is audited as `mail.inject` (ids, format, size and
mailbox, no content).

## Provider API mocks

Applications that send through an email provider's HTTP API can send to FastMock instead, with the provider's
official SDK. Each provider lives under `/__mail/<provider>/` on the **data port**; set the SDK's base URL to it.
The API paths below that prefix are the provider's own.

| Provider | Base URL | Endpoints |
|---|---|---|
| Amazon SES | `http://localhost:8080/__mail/ses` | `POST /v2/email/outbound-emails`, `/v2/email/outbound-bulk-emails`; `POST /` with `Action=SendEmail` or `SendRawEmail` (v1) |
| SendGrid | `http://localhost:8080/__mail/sendgrid` | `POST /v3/mail/send` |
| Mailgun | `http://localhost:8080/__mail/mailgun` | `POST /v3/{domain}/messages`, `/v3/{domain}/messages.mime` |
| Postmark | `http://localhost:8080/__mail/postmark` | `POST /email`, `/email/withTemplate`, `/email/batch`, `/email/batchWithTemplates` |
| Resend | `http://localhost:8080/__mail/resend` | `POST /emails` |
| Brevo | `http://localhost:8080/__mail/brevo` | `POST /v3/smtp/email` |

To store the messages in a space regardless of the recipients, add `@<space>` after the provider:
`http://localhost:8080/__mail/sendgrid/@billing`. Without it, messages are routed by recipient domain like SMTP
mail. An unknown space answers the provider's own `404`.

Each sent message is built into MIME and stored with `source: provider`, `provider`, and `providerExtras`: the id
the API answered with, the template id and template data, tags and metadata, and `testMode` for messages sent in
the provider's sandbox or test mode. Provider-side templates are not rendered (FastMock does not have them); SES
templates are rendered when the request carries `TemplateContent`. Attachments given by URL (Resend `path`,
Brevo `url`) are not fetched and are stored empty.

Any non-empty API key is accepted, and a call without one gets the provider's `401`. SES accepts any well-formed
SigV4 or SigV4a signature; use dummy credentials such as `test`/`test` and any region. To accept only certain keys
in a space, list their SHA-256 hex digests in `apiKeys` of the [webhook settings](#provider-webhooks) (SES: the
access key id); other keys get the provider's invalid-key error.

`/__mail/` is reserved while mail is on: stubs never answer it and these requests are never counted as
unmatched. They appear in the request journal with `source: mail`, and a response carries
`x-fastmock-chaos: <rule id>` when a [chaos rule](#provider-errors) fired.

!!! note "SDK versions"
    The snippets use each SDK's documented base-URL option. SDKs change these options between major versions;
    check the reference of the version you use. SDKs that take a host name instead of a URL (SendGrid Java,
    Postmark Node and Java) get the host with the `/__mail/<provider>` path appended.

### Amazon SES

The v2 API (`sesv2`) and the v1 query API (`ses`, used by older SDKs and many libraries) share one base URL.
Without code changes, the AWS SDKs for JavaScript v3, Java v2, Python and Go v2 read
`AWS_ENDPOINT_URL_SESV2` (v2) and `AWS_ENDPOINT_URL_SES` (v1):

```sh
export AWS_ENDPOINT_URL_SESV2=http://localhost:8080/__mail/ses
export AWS_ACCESS_KEY_ID=test AWS_SECRET_ACCESS_KEY=test AWS_REGION=us-east-1
```

=== "Node"

    ```js
    import { SESv2Client, SendEmailCommand } from "@aws-sdk/client-sesv2";

    const ses = new SESv2Client({
      endpoint: "http://localhost:8080/__mail/ses",
      region: "us-east-1",
      credentials: { accessKeyId: "test", secretAccessKey: "test" },
    });
    ```

=== "Java"

    ```java
    SesV2Client ses = SesV2Client.builder()
        .endpointOverride(URI.create("http://localhost:8080/__mail/ses"))
        .region(Region.US_EAST_1)
        .credentialsProvider(StaticCredentialsProvider.create(AwsBasicCredentials.create("test", "test")))
        .build();
    ```

=== "Python"

    ```python
    ses = boto3.client("sesv2", endpoint_url="http://localhost:8080/__mail/ses", region_name="us-east-1",
                       aws_access_key_id="test", aws_secret_access_key="test")
    ```

=== "Go"

    ```go
    cfg, _ := config.LoadDefaultConfig(ctx, config.WithRegion("us-east-1"),
        config.WithCredentialsProvider(credentials.NewStaticCredentialsProvider("test", "test", "")))
    ses := sesv2.NewFromConfig(cfg, func(o *sesv2.Options) {
        o.BaseEndpoint = aws.String("http://localhost:8080/__mail/ses")
    })
    ```

| Endpoint | Success |
|---|---|
| `SendEmail` (v2) | `200 {"MessageId": "..."}` with `x-amzn-RequestId` |
| `SendBulkEmail` (v2) | `200 {"BulkEmailEntryResults": [...]}`, one result per entry |
| `SendEmail`, `SendRawEmail` (v1) | `200` XML `SendEmailResponse` / `SendRawEmailResponse` with `MessageId` |

Simple, raw and template content are supported; Bcc recipients stay in the envelope only; `EmailTags` are stored
as metadata. v1 calls take at most 50 recipients. A call without a signature gets `403 IncompleteSignature`. SES
has no sandbox flag in the API.

### SendGrid

=== "Node"

    ```js
    import sgMail from "@sendgrid/mail";

    sgMail.setApiKey("SG.test");
    sgMail.client.setDefaultRequest("baseUrl", "http://localhost:8080/__mail/sendgrid");
    ```

    Call `setDefaultRequest` after `setApiKey`, which resets the base URL.

=== "Java"

    ```java
    SendGrid sg = new SendGrid("SG.test", true);
    sg.setHost("localhost:8080/__mail/sendgrid");
    ```

    The second constructor argument switches the SDK to plain `http`.

=== "Python"

    ```python
    sg = SendGridAPIClient("SG.test", host="http://localhost:8080/__mail/sendgrid")
    ```

=== "Go"

    ```go
    client := sendgrid.NewSendClient("SG.test")
    client.BaseURL = "http://localhost:8080/__mail/sendgrid/v3/mail/send"
    ```

`POST /v3/mail/send` answers `202` with an empty body and `X-Message-Id`. Each personalization becomes one
message; gzip request bodies, categories (at most 10) and legacy `substitutions` work; reserved headers are
refused like SendGrid does. With `mail_settings.sandbox_mode.enable: true` the call answers `200`, and the
message is stored with `testMode: true`.

### Mailgun

=== "Node"

    ```js
    import formData from "form-data";
    import Mailgun from "mailgun.js";

    const mg = new Mailgun(formData).client({
      username: "api",
      key: "test-key",
      url: "http://localhost:8080/__mail/mailgun",
    });
    ```

=== "Java"

    ```java
    MailgunMessagesApi mailgun = MailgunClient.config("http://localhost:8080/__mail/mailgun", "test-key")
        .createApi(MailgunMessagesApi.class);
    ```

=== "Python"

    ```python
    from mailgun.client import Client

    client = Client(auth=("api", "test-key"), api_url="http://localhost:8080/__mail/mailgun")
    ```

    The Python SDK allows plain `http` only for `localhost` and `127.0.0.1`; for another host, serve FastMock
    over https with a trusted certificate.

=== "Go"

    ```go
    mg := mailgun.NewMailgun("test-key")
    mg.SetAPIBase("http://localhost:8080/__mail/mailgun")
    ```

    The base has no version suffix; the SDK adds `/v3`. `MG_URL` sets it without code changes.

`POST /v3/{domain}/messages` (multipart or urlencoded) and `/messages.mime` answer
`200 {"id": "<20261121091403.5c0e@example.com>", "message": "Queued. Thank you."}`. `recipient-variables` sends
one message per recipient with `%recipient.x%` replaced; `o:`, `h:`, `v:` and `t:` values together may hold at
most 16 KB; templates are stored as `name@version`. `o:testmode=yes` marks the message `testMode`.

### Postmark

=== "Node"

    ```js
    import * as postmark from "postmark";

    const client = new postmark.ServerClient("server-token", {
      requestHost: "localhost:8080/__mail/postmark",
      useHttps: false,
    });
    ```

=== "Java"

    ```java
    ApiClient client = Postmark.getApiClient("server-token", false, "localhost:8080/__mail/postmark");
    ```

=== "Python"

    ```python
    client = ServerClient("server-token", base_url="http://localhost:8080/__mail/postmark")
    ```

=== "Go"

    Postmark has no official Go SDK; with `github.com/mrz1836/postmark`:

    ```go
    client := postmark.NewClient("server-token", "")
    client.BaseURL = "http://localhost:8080/__mail/postmark"
    ```

`POST /email` and `/email/withTemplate` answer
`200 {"To": "...", "SubmittedAt": "...", "MessageID": "...", "ErrorCode": 0, "Message": "OK"}`; the batch
endpoints answer an array with one result per message and their own `ErrorCode`. At most 50 recipients per
message and 500 messages per batch. The token `POSTMARK_API_TEST` is always accepted, answers
`"Message": "Test job accepted"` and marks the message `testMode`.

### Resend

=== "Node"

    ```sh
    export RESEND_BASE_URL=http://localhost:8080/__mail/resend
    ```

    ```js
    import { Resend } from "resend";

    const resend = new Resend("re_test");
    ```

=== "Java"

    ```java
    Resend resend = Resend.builder()
        .apiKey("re_test")
        .baseUrl("http://localhost:8080/__mail/resend")
        .build();
    ```

=== "Python"

    ```python
    import resend

    resend.api_key = "re_test"
    resend.api_url = "http://localhost:8080/__mail/resend"
    ```

=== "Go"

    ```sh
    export RESEND_BASE_URL=http://localhost:8080/__mail/resend/
    ```

    ```go
    client := resend.NewClient("re_test")
    ```

    The Go SDK needs the trailing `/`.

`POST /emails` answers `200 {"id": "..."}`. A `User-Agent` header is required (`403` without it), as at Resend.
`Idempotency-Key` returns the same `id` for the same body within 24 hours and answers
`409 invalid_idempotent_request` for a different body.

### Brevo

The base URL includes `/v3`.

=== "Node"

    ```js
    import { BrevoClient } from "@getbrevo/brevo";

    const brevo = new BrevoClient({ apiKey: "xkeysib-test", baseUrl: "http://localhost:8080/__mail/brevo/v3" });
    ```

=== "Java"

    ```java
    ApiClient client = Configuration.getDefaultApiClient();
    client.setBasePath("http://localhost:8080/__mail/brevo/v3");
    ```

=== "Python"

    ```python
    client = Brevo(api_key="xkeysib-test", base_url="http://localhost:8080/__mail/brevo/v3")
    ```

=== "Go"

    ```go
    cfg := brevo.NewConfiguration()
    cfg.BasePath = "http://localhost:8080/__mail/brevo/v3"
    cfg.AddDefaultHeader("api-key", "xkeysib-test")
    client := brevo.NewAPIClient(cfg)
    ```

`POST /v3/smtp/email` answers `201 {"messageId": "..."}`, or `{"messageIds": [...]}` with `messageVersions`,
and `202` when `scheduledAt` is set.

### Provider errors

Provider error answers come from [mail chaos rules](#chaos-for-mail) with `stage: provider` and an
`action.providerError` name. FastMock answers in the provider's own format, status code and rate-limit headers,
so the SDK raises its usual exception:

```json
{"rules": [
  {"id": "sendgrid-429", "stage": "provider", "match": {"provider": {"equalTo": "sendgrid"}},
   "every": 2, "action": {"providerError": "rateLimit"}},
  {"id": "ses-rejected", "stage": "provider", "match": {"rcptTo": {"contains": "blocked@"}},
   "action": {"providerError": "MessageRejected"}}
]}
```

| Names | Providers |
|---|---|
| `invalidKey`, `forbidden`, `validation`, `messageRejected`, `rateLimit`, `quota`, `templateNotFound`, `tooLarge`, `serverError`, `unavailable` | all |
| native error codes: v2 `MessageRejected`, `NotFoundException`, `TooManyRequestsException`, `LimitExceededException`, ...; v1 `Throttling`, `MailFromDomainNotVerified`, `ConfigurationSetDoesNotExist`, ... | SES |
| `recipientsRateLimit` (HTTP 420) | Mailgun |
| `inactiveRecipient`, `accountPending`, or any number as the `ErrorCode` (answered with `422`) | Postmark |
| native names such as `validation_error`, `missing_api_key`, `rate_limit_exceeded`, `daily_quota_exceeded` | Resend |
| `credits` (HTTP 402) | Brevo |

Names are matched ignoring case. An unknown name answers the provider's `500` with a message that starts with
`FastMock: unknown providerError`. A rule is evaluated once per API call, also for batch calls.

## Provider webhooks

FastMock can send the webhooks a provider sends to your application: delivery events after a message was
accepted, and inbound mail. Configure them per space:

```sh
curl -s -X PUT "localhost:9090/__admin/mail/webhooks?space=billing" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "webhooks": [
    {"id": "sendgrid-events", "provider": "sendgrid", "kind": "events",
     "url": "https://app.example.com/hooks/sendgrid", "events": ["delivered", "bounce"]},
    {"id": "mailgun-events", "provider": "mailgun", "kind": "events",
     "url": "https://app.example.com/hooks/mailgun", "signingKey": "test-webhook-signing-key"},
    {"id": "postmark-inbound", "provider": "postmark", "kind": "inbound",
     "url": "https://hooks:test-password@app.example.com/hooks/postmark/inbound"},
    {"id": "ses-events", "provider": "ses", "kind": "events",
     "url": "https://app.example.com/hooks/ses", "topicArn": "arn:aws:sns:us-east-1:123456789012:billing-mail"}
  ],
  "apiKeys": {"sendgrid": ["9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"]}
}'
```

| Field | Meaning |
|---|---|
| `id` | Letters, digits, `.`, `_`, `-`; `webhook-<n>` when left out. |
| `provider` | `ses`, `sendgrid`, `mailgun` or `postmark`. |
| `kind` | `events` (delivery events) or `inbound` (inbound mail). |
| `url` | The application's `http(s)` URL. User information in the URL (`https://user:pass@host/...`) is sent as basic authentication. Checked against the [egress policy](../operations/security.md#egress-policy). |
| `signingKey` | Write-only. SendGrid: a P-256 private key (PKCS#8, PEM or base64) to sign with instead of the space's generated key. Mailgun: the webhook signing key. `""` removes a stored key. |
| `events` | `delivered`, `bounce`, `complaint`, `deferred`, `dropped`, `open`, `click`; all when empty. |
| `raw` | SendGrid inbound: post the full MIME message (`email` field) instead of parsed fields. |
| `topicArn` | SES: the SNS topic ARN in notifications. |

At most 20 webhooks and 20 expected keys per provider. `GET` (`mail:read`) never shows signing keys: it shows
`hasSigningKey`, URL passwords as `***`, a `publicKey` for each SendGrid webhook and the space's
`sendgridPublicKey`. A webhook `PUT` again with the same `id` and without `signingKey` (or with `***` as the URL
password) keeps the stored value. Changes are audited as `mail.webhooks.set` and `mail.webhooks.clear`.

### Delivery events

After a provider API call stores a message, FastMock sends one event per recipient to the space's `events`
webhooks of that provider:

| Recipient local part (without `+tag`) | Event |
|---|---|
| `bounce` | `bounce` (Postmark `HardBounce`) |
| `complaint` | `complaint` (SendGrid `spamreport`, Postmark `SpamComplaint`) |
| `deferred` | `deferred` (Postmark `SoftBounce`) |
| `dropped` | `dropped` (Postmark `ManuallyDeactivated`) |
| anything else | `delivered` |

So `bounce@example.test` produces a bounce. `open` and `click` are accepted in the settings but never sent.
Messages sent in the provider's test mode produce no events. Events go through the durable
[callbacks outbox](callbacks.md): up to 8 attempts, retried on `5xx`, `408`, `429` and connection errors, visible
in `GET /__admin/callbacks` with source `mail` and the message id as correlation.

Configure the application's verification against FastMock's keys:

| Provider | Payload | What to configure in the application |
|---|---|---|
| SendGrid | JSON array of events, `custom_args` at the top level, signed with ECDSA in `X-Twilio-Email-Event-Webhook-Signature` and `-Timestamp` | Use the space's `sendgridPublicKey` (or the webhook's `publicKey`) as the Event Webhook verification key; the SDK's verification helpers then verify FastMock's signatures. |
| Mailgun | one call per recipient, `{signature, event-data}` with HMAC-SHA256 | Set the same webhook signing key in the application and as the webhook's `signingKey`. |
| Postmark | `Delivery`, `Bounce` or `SpamComplaint` JSON | Postmark does not sign webhooks; put the basic-auth user and password the application expects into the URL. |
| SES | an SNS `Notification` per event, `Signature: FASTMOCK-UNSIGNED`, no `SubscriptionConfirmation` | SNS signatures are made with AWS's private keys, and validators fetch the certificate from AWS, so FastMock cannot produce a signature they accept. Switch SNS signature verification off in the application's test configuration, or test the handler below the HTTP layer. |

### Inbound mail

`POST /__admin/mail/inbound` (`mail:write`) turns a message into the provider's inbound webhook and sends it to
the space's `inbound` webhooks of that provider. The message is not stored.

```sh
curl -s -X POST "localhost:9090/__admin/mail/inbound?space=billing" \
  -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{
  "provider": "postmark",
  "message": {"from": "Alice <alice@example.com>", "to": ["support@billing.example.test"],
              "subject": "Re: order 42", "text": "Thanks!"}
}'
```

```json
{"deliveries": [{"webhookId": "postmark-inbound", "callbackId": "019a9c51-0d2e-7f10-8a44-3b6c1d2e9f00"}]}
```

Instead of `message`, give `raw` (MIME text) or `rawBase64`, or post `Content-Type: message/rfc822` with
`?provider=...`. `mailFrom` and `rcptTo` override the envelope; `webhookId` limits it to one webhook. `404` when
the space has no inbound webhook for the provider. Payloads: SendGrid Inbound Parse multipart (ECDSA-signed over
the exact body), Mailgun route form fields with the HMAC fields (`body-mime` when the URL ends in `mime`, JSON with
`X-Mailgun-*` headers when it ends in `json`), Postmark inbound JSON, SES `Received` notification in an SNS
envelope. Audited as `mail.inbound`.

## Mail in the web UI

The [web UI](ui.md) is getting a Mail page, shown when mail is enabled and the user holds `mail:read`:

- a message list with the same search syntax as the API, live updates as messages arrive, unread markers and
  labels;
- a message view with tabs for the HTML (in a sandboxed frame), text, headers, raw source, attachments, SMTP
  transcript, and links and codes, plus provider and template details;
- deleting messages, and an inject dialog for users with `mail:write`;
- views of mailbox passwords, webhook settings and mail chaos rules, editable with `mail:write`;
- a card layout on phones.

## Limits and retention

| Limit | Value |
|---|---|
| messages kept per space (and in the global area) | 1000, oldest dropped first (`FASTMOCK_MAIL_MAX_MESSAGES`) |
| age of a message | 7 days (`FASTMOCK_MAIL_MAX_AGE`) |
| message size, also for injected and provider messages | 10 MiB (`FASTMOCK_MAIL_MAX_MESSAGE_BYTES`, at most 512 MiB) |
| MIME parsing | nesting depth 16, 500 parts; larger or malformed messages are kept raw and marked `unparsed` |
| labels | 32 per message, 64 characters each |
| search query | 1024 bytes, 32 terms |
| list page | 1 to 500 messages |
| wait | 1 to 500 messages, 60 s, 10,000 stored messages scanned; 4 scans at a time per replica (`503` beyond) |
| event streams (`GET /__admin/mail/stream`) | 512 per replica |
| inline images in `/html` | 4 MiB of `data:` URIs |
| code pattern | 256 bytes, at most 100 codes |
| SMTP transcript | 500 lines of 512 characters |
| chaos rules | 100 per area; delay at most 10 minutes |
| webhooks | 20 per area; 20 expected keys per provider |
| IMAP folders | 100 per mailbox |
| provider calls | 128 MiB of built messages per call, 10,000 form fields |

Retention runs once a minute on one replica at a time. The memory store also drops the oldest messages beyond the
per-space limit as new ones arrive; with PostgreSQL an area can exceed the limit until the next run. Removed
messages are announced as `message.deleted` on the event stream.

The admin API's `GET /__admin/mail/stream` is a Server-Sent Events stream of `message.received` and
`message.deleted` events for the areas the caller may read, with a `: heartbeat` comment every
`heartbeatSeconds` (1 to 60, default 15) and a `resync` event when events were missed.

## Metrics

| Metric | Type | Labels | Meaning |
|---|---|---|---|
| `fastmock_mail_messages_total` | counter | `source`, `provider` | Messages stored (one per stored copy). |
| `fastmock_mail_smtp_sessions_total` | counter | `result` (`quit`, `disconnected`, `timeout`, `rejected`, `chaos`, `shutdown`, `error`) | Finished SMTP sessions. |
| `fastmock_mail_imap_sessions_total`, `fastmock_mail_pop3_sessions_total` | counter | `result` | Finished IMAP and POP3 sessions. |
| `fastmock_mail_rejected_total` | counter | `reason` | Refused commands, logins and connections (`auth_failed`, `auth_required`, `auth_before_tls`, `size`, `recipients`, `fail_address`, `chaos`, `timeout`, `connections`, ...). |
| `fastmock_mail_chaos_injections_total` | counter | `stage` | Mail chaos rule firings. |
| `fastmock_mail_connections` | gauge | | Open SMTP, IMAP and POP3 connections. |
| `fastmock_mail_provider_requests_total` | counter | `provider`, `status_class` | Provider API calls. |
| `fastmock_mail_webhooks_total` | counter | `provider`, `kind` | Webhooks queued in the outbox. |
| `fastmock_mail_waits_total` | counter | `result` (`found`, `timeout`) | Finished waits. |
| `fastmock_mail_waits_active`, `fastmock_mail_streams_active` | gauge | | Waits and event streams in progress. |
| `fastmock_mail_messages`, `fastmock_mail_storage_bytes` | gauge | `space` | Stored messages and their raw size per area (PostgreSQL only; cluster-wide, aggregate with `max`). |

The full list, with the storage and event metrics, is in [Observability](../operations/observability.md#mail).

## Troubleshooting

| Symptom | Cause and fix |
|---|---|
| Connection refused on port 2525 | `FASTMOCK_MAIL_ENABLED` is not `true`, the port is not published (Docker `-p`, the mail Service in Kubernetes), or `FASTMOCK_MAIL_SMTP_LISTEN` is empty. |
| `530 5.7.0 Authentication required` | `FASTMOCK_MAIL_SMTP_REQUIRE_AUTH=true` and the client did not authenticate. |
| `535 5.7.8 Authentication credentials invalid` | The user is not in `FASTMOCK_MAIL_SMTP_USERS` with the hash of that password, or the managed token lacks `mail:write` globally or in the space named like the user. |
| `538 5.7.11 Encryption required ...`, or the client finds no `AUTH` | A certificate is configured, so AUTH needs STARTTLS first. Enable STARTTLS in the client, or set `FASTMOCK_MAIL_ALLOW_PLAINTEXT_AUTH=true` for a test network. |
| `502 5.5.1 STARTTLS is not offered`, or "server does not support STARTTLS" | The client requires TLS and no certificate is configured. Configure one or let the client send without TLS. |
| Certificate errors in the client | The client does not trust FastMock's certificate; add its CA to the client's trust store. |
| `552 5.3.4 Message size exceeds fixed maximum message size` | Raise `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`. |
| `452 4.5.3 Too many recipients` | Raise `FASTMOCK_MAIL_MAX_RECIPIENTS`; clients usually retry the remaining recipients in another transaction. |
| `553 5.6.7 Non-ASCII address needs SMTPUTF8` | An internationalised address was sent without the `SMTPUTF8` parameter; enable SMTPUTF8 in the client. |
| `421 4.7.0 Too many connections, try again later` | More than 32 connections from one address or 256 in total; reduce the client's pool size or raise the limits. |
| `421 4.4.2 Timeout waiting for the client` | A pooled connection stayed idle longer than `FASTMOCK_MAIL_IDLE_TIMEOUT`; clients reconnect, or raise the timeout. |
| Unexpected `4xx`/`5xx` or dropped connections with `FastMock chaos rule ...` | A mail chaos rule fired. `GET /__admin/mail/chaos?space=...` (and without `space` for `connect`, `tls`, `ehlo`) shows the rules and counters. A `fail+<code>@` recipient does the same without a rule. |
| The message is not in the expected space | Check `envelope.rcptTo` and `envelope.authUser`: routing follows `mailDomains`, then the AUTH user name. Messages of the global area need global `mail:read`. |
| `wait` answers `408` although the message is listed | It arrived before the wait started. Pass `since` or `includeExisting: true`. |
| IMAP `NO [AUTHENTICATIONFAILED]`, POP3 `-ERR [AUTH] Invalid credentials` | The token lacks `mail:read` in the space that claims the address's domain, or the mailbox password was set in another space. |
| IMAP `NO [PRIVACYREQUIRED]`, POP3 `... use STLS first` | Enable STARTTLS/STLS in the client, use the implicit-TLS port, or allow plaintext logins. |
| A provider SDK gets `404` `FastMock does not implement ...` | The base URL lacks `/__mail/<provider>`, or has one path segment too many (Brevo needs `/v3` in the base URL, the others not). |
| Resend SDK calls answer `403` | The request has no `User-Agent` header. |
| Webhooks never arrive | Check `GET /__admin/callbacks` (source `mail`): the URL may be refused by the egress policy, the receiver may answer errors, or the message was sent in test mode. |
