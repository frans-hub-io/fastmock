import { Alert, Button, FileButton, FileInput, Group, Modal, SegmentedControl, Stack, TagsInput, Text, Textarea, TextInput } from '@mantine/core';
import { IconAlertTriangle, IconFileUpload, IconMailPlus, IconPaperclip } from '@tabler/icons-react';
import { useMediaQuery } from '@mantine/hooks';
import { useState } from 'react';
import { mailApi, type ComposeAttachment, type ComposeRequest } from '../../api/mail';
import { useApiMutation } from '../../api/mutations';
import { useSpace, useSpaceChoice, useSpaceOptions } from '../../auth/useAuth';
import { SpaceField } from '../../components/spaces';
import { formatBytes } from '../../lib/format';
import { bareAddress, composeIssues, domainOf, fileToBase64, mailDomainOwner, rawIssue, rawRecipient, type ComposeIssues } from '../../lib/mail';

const EMPTY: ComposeRequest = { from: '', to: [], cc: [], subject: '', text: '', html: '', attachments: [] };

/** The compose request without empty optional fields. */
function cleanCompose(c: ComposeRequest): ComposeRequest {
  const out: ComposeRequest = { from: c.from.trim(), to: c.to.map((a) => a.trim()) };
  if (c.cc?.length) out.cc = c.cc.map((a) => a.trim());
  if (c.subject?.trim()) out.subject = c.subject;
  if (c.text?.trim()) out.text = c.text;
  if (c.html?.trim()) out.html = c.html;
  if (c.attachments?.length) out.attachments = c.attachments;
  return out;
}

/** Puts a message into the space as if it had been received (`POST /__admin/mail/messages`): composed or raw. */
export function InjectDialog({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const [mode, setMode] = useState<'compose' | 'raw'>('compose');
  const [c, setC] = useState<ComposeRequest>(EMPTY);
  const [files, setFiles] = useState<File[]>([]);
  const [raw, setRaw] = useState('');
  const [mailFrom, setMailFrom] = useState('');
  const [rcptTo, setRcptTo] = useState('');
  const [touched, setTouched] = useState(false);
  const [chosen, choose] = useSpaceChoice('mail:write');
  const [picked, setPicked] = useState(false);
  const spaces = useSpace();
  const writable = useSpaceOptions('mail:write');
  const [fileError, setFileError] = useState<string | null>(null);
  const phone = useMediaQuery('(max-width: 47.99em)');

  const reset = () => {
    setC(EMPTY);
    setFiles([]);
    setRaw('');
    setMailFrom('');
    setRcptTo('');
    setTouched(false);
    setFileError(null);
    setPicked(false);
  };

  const recipient = mode === 'compose' ? bareAddress(c.to[0] ?? c.cc?.[0]) : (bareAddress(rcptTo) ?? rawRecipient(raw));
  const domain = recipient ? domainOf(recipient) : undefined;
  const owner = spaces.enabled && domain ? mailDomainOwner(domain, spaces.spaces) : null;
  const space = !picked && owner && writable.includes(owner) ? owner : chosen;
  const spaceHint =
    spaces.enabled && recipient && domain && space !== owner
      ? owner
        ? `${domain} belongs to ${owner}: IMAP and POP3 logins as this address see only ${owner}.`
        : `No space owns ${domain}: IMAP and POP3 log in to this copy as ${space ?? '_global'}/${recipient}.`
      : null;

  const send = useApiMutation<{ compose?: ComposeRequest; raw?: string }>({
    fn: (v) => (v.compose ? mailApi.inject(v.compose, space) : mailApi.injectRaw(v.raw ?? '', { space, mailFrom, rcptTo })),
    invalidate: [['mail', 'list']],
    success: 'Message injected',
    errorTitle: 'Injecting the message failed',
    onSuccess: () => {
      reset();
      onClose();
    },
  });

  const attachmentsSize = (c.attachments ?? []).reduce((n, a) => n + Math.floor((a.base64.length * 3) / 4), 0);
  const issues: ComposeIssues = mode === 'compose' ? composeIssues(c) : {};
  const rawErr = mode === 'raw' ? rawIssue(raw) : undefined;
  const blocked = mode === 'compose' ? Object.keys(issues).length > 0 : !!rawErr;
  const show = <K extends keyof ComposeIssues>(k: K) => (touched ? issues[k] : undefined);

  const attach = async (picked: File[]) => {
    setFileError(null);
    try {
      const list: ComposeAttachment[] = await Promise.all(
        picked.map(async (f) => ({ name: f.name, contentType: f.type || 'application/octet-stream', base64: await fileToBase64(f) })),
      );
      setFiles(picked);
      setC((x) => ({ ...x, attachments: list }));
    } catch (e) {
      setFileError(e instanceof Error ? e.message : String(e));
    }
  };

  return (
    <Modal opened={opened} onClose={onClose} title="Inject a message" size="xl" fullScreen={phone} data-testid="mail-inject-dialog">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          setTouched(true);
          if (blocked) return;
          send.mutate(mode === 'compose' ? { compose: cleanCompose(c) } : { raw: raw.replace(/\r?\n/g, '\r\n') });
        }}
        noValidate
      >
        <Stack gap="md">
          <Text size="sm" c="dimmed">
            The message is stored as if it had been received (source <code>inject</code>) and filed into the IMAP and POP3 inbox of each recipient. Nothing is delivered anywhere.
          </Text>
          <Group justify="space-between" wrap="wrap" gap="sm">
            <SegmentedControl
              value={mode}
              onChange={(v) => {
                setMode(v);
                setTouched(false);
              }}
              data={[
                { label: 'Compose', value: 'compose' },
                { label: 'Raw message (EML)', value: 'raw' },
              ]}
              aria-label="Message format"
              data-testid="inject-mode"
            />
            <SpaceField
              perm="mail:write"
              value={space}
              onChange={(v) => {
                setPicked(true);
                choose(v);
              }}
              description={owner && space === owner && !picked ? `Owner of ${domain ?? ''}` : 'Where the message is stored'}
            />
          </Group>
          {spaceHint && (
            <Group gap={6} wrap="nowrap" align="flex-start" c="var(--fh-semantic-warning-fg)" data-testid="inject-space-hint">
              <IconAlertTriangle size={14} style={{ flexShrink: 0, marginTop: 2 }} aria-hidden />
              <Text size="xs" c="inherit">
                {spaceHint}
              </Text>
            </Group>
          )}
          {mode === 'compose' ? (
            <>
              <div className="fm-form-grid">
                <TextInput
                  label="From"
                  placeholder="Shop <shop@example.com>"
                  value={c.from}
                  onChange={(e) => setC({ ...c, from: e.currentTarget.value })}
                  error={show('from')}
                  required
                  className="fm-span-2"
                  data-testid="inject-from"
                />
                <TextInput label="Subject" placeholder="Your code" value={c.subject ?? ''} onChange={(e) => setC({ ...c, subject: e.currentTarget.value })} error={show('subject')} className="fm-span-2" data-testid="inject-subject" />
                <TagsInput
                  label="To"
                  placeholder="alice@example.test, Enter to add"
                  value={c.to}
                  onChange={(v) => setC({ ...c, to: v })}
                  error={show('to')}
                  required
                  splitChars={[',', ' ', ';']}
                  acceptValueOnBlur
                  className="fm-span-2"
                  data-testid="inject-to"
                />
                <TagsInput label="Cc" placeholder="Optional" value={c.cc ?? []} onChange={(v) => setC({ ...c, cc: v })} error={show('cc')} splitChars={[',', ' ', ';']} acceptValueOnBlur className="fm-span-2" data-testid="inject-cc" />
              </div>
              <Textarea label="Text body" placeholder="Your code is 123456" autosize minRows={3} maxRows={10} value={c.text ?? ''} onChange={(e) => setC({ ...c, text: e.currentTarget.value })} data-testid="inject-text" />
              <Textarea
                label="HTML body"
                description="Shown in the HTML tab inside a sandbox: scripts never run"
                placeholder="<p>Your code is <b>123456</b></p>"
                autosize
                minRows={3}
                maxRows={10}
                value={c.html ?? ''}
                onChange={(e) => setC({ ...c, html: e.currentTarget.value })}
                classNames={{ input: 'mono' }}
                data-testid="inject-html"
              />
              <FileInput
                label="Attachments"
                placeholder="Choose files"
                multiple
                clearable
                value={files}
                onChange={(f) => {
                  if (!f.length) {
                    setFiles([]);
                    setC({ ...c, attachments: [] });
                  } else void attach(f);
                }}
                leftSection={<IconPaperclip size={16} />}
                description={files.length ? `${String(files.length)} file${files.length === 1 ? '' : 's'}, ${formatBytes(attachmentsSize)}` : 'Sent base64-encoded with the message'}
                error={fileError ?? show('size')}
                data-testid="inject-attachments"
              />
              {touched && issues.body && (
                <Alert color="yellow" icon={<IconAlertTriangle size={18} />} p="xs" data-testid="inject-body-issue">
                  {issues.body}
                </Alert>
              )}
            </>
          ) : (
            <>
              <Group justify="space-between" align="flex-end" wrap="wrap">
                <Text size="sm" fw={500}>
                  Raw message
                </Text>
                <FileButton
                  accept="message/rfc822,.eml,text/plain"
                  onChange={(f) => {
                    if (f) void f.text().then(setRaw);
                  }}
                >
                  {(props) => (
                    <Button {...props} size="compact-sm" variant="light" leftSection={<IconFileUpload size={14} />}>
                      Load .eml file
                    </Button>
                  )}
                </FileButton>
              </Group>
              <Textarea
                aria-label="Raw message"
                placeholder={'From: shop@example.com\nTo: alice@example.test\nSubject: Hello\n\nIt works.'}
                autosize
                minRows={8}
                maxRows={18}
                value={raw}
                onChange={(e) => setRaw(e.currentTarget.value)}
                error={touched ? rawErr : undefined}
                classNames={{ input: 'mono' }}
                data-testid="inject-raw"
              />
              <div className="fm-form-grid">
                <TextInput label="Envelope sender" description="Default: the From address" placeholder="shop@example.com" value={mailFrom} onChange={(e) => setMailFrom(e.currentTarget.value)} className="fm-span-2" />
                <TextInput label="Envelope recipients" description="Comma-separated; default: To, Cc and Bcc" placeholder="alice@example.test" value={rcptTo} onChange={(e) => setRcptTo(e.currentTarget.value)} className="fm-span-2" />
              </div>
            </>
          )}
          <Group justify="flex-end" gap="xs">
            <Button variant="subtle" color="gray" onClick={onClose}>
              Cancel
            </Button>
            <Button type="submit" leftSection={<IconMailPlus size={16} />} loading={send.isPending} data-testid="inject-submit">
              Inject message
            </Button>
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}
