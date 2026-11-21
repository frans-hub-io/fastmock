import { Alert, Button, Group, Modal, PasswordInput, SegmentedControl, Stack, Table, Text, TextInput } from '@mantine/core';
import { IconAlertTriangle, IconInbox, IconKey, IconKeyOff, IconLock, IconMailbox } from '@tabler/icons-react';
import { useMediaQuery } from '@mantine/hooks';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { mailApi, useMailFeature, useMailServer, type MailboxView } from '../../api/mail';
import { confirmAction, useApiMutation } from '../../api/mutations';
import { useCan, useSpace, useSpaceQuery } from '../../auth/useAuth';
import { Copy, DataTable, DetailList, SectionCard, ToneBadge } from '../../components/common';
import { Denied } from '../../components/Permitted';
import { SpaceBadge } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { missingText } from '../../auth/permissions';
import { docsUrl } from '../../lib/docs';
import { endpointText, loginRule, mailEndpoints, randomPassword, type MailEndpoint } from '../../lib/mail';
import classes from './Mail.module.css';

function PasswordDialog({ box, onClose }: { box: MailboxView | null; onClose: () => void }) {
  const [mode, setMode] = useState<'generate' | 'own'>('generate');
  const [own, setOwn] = useState('');
  const [shown, setShown] = useState<string | null>(null);
  const close = () => {
    setOwn('');
    setShown(null);
    setMode('generate');
    onClose();
  };
  const set = useApiMutation<string>({
    fn: (pw) => mailApi.setPassword(box?.address ?? '', pw, box?.space ?? null),
    invalidate: [['mail', 'mailboxes']],
    errorTitle: 'Setting the password failed',
    onSuccess: (_d, pw) => setShown(pw),
  });
  const ownError = own && new TextEncoder().encode(own).length > 1024 ? 'At most 1024 bytes' : undefined;
  return (
    <Modal opened={!!box} onClose={close} title={shown ? 'Password set' : `Set the password of ${box?.address ?? ''}`} size="lg" data-testid="mailbox-password-dialog">
      {shown ? (
        <Stack gap="sm">
          <Alert color="yellow" icon={<IconAlertTriangle size={18} />} title="Copy it now">
            This is the only time the password is shown. FastMock stores only a salted hash of it.
          </Alert>
          <DetailList
            columns={1}
            items={[
              { label: 'User name', value: <span className="mono break-all">{box?.address}</span> },
              {
                label: 'Password',
                value: (
                  <Group gap={4} wrap="nowrap" className="fm-cmd">
                    <span className="mono break-all token-value" data-testid="mailbox-password-value">
                      {shown}
                    </span>
                    <Copy value={shown} label="Copy password" />
                  </Group>
                ),
              },
            ]}
          />
          <Group justify="flex-end">
            <Button onClick={close} data-testid="mailbox-password-done">
              I copied it
            </Button>
          </Group>
        </Stack>
      ) : (
        <form
          onSubmit={(e) => {
            e.preventDefault();
            if (mode === 'own' && (!own || ownError)) return;
            set.mutate(mode === 'own' ? own : randomPassword());
          }}
        >
          <Stack gap="sm">
            <Text size="sm" c="dimmed">
              IMAP and POP3 clients log in to this mailbox with its address and this password. A managed token with mail:read keeps working as the password too.
              {box?.hasPassword ? ' The current password stops working.' : ''}
            </Text>
            <SegmentedControl
              value={mode}
              onChange={(v) => setMode(v)}
              data={[
                { label: 'Generate one', value: 'generate' },
                { label: 'Enter my own', value: 'own' },
              ]}
              aria-label="Password source"
            />
            {mode === 'own' && <PasswordInput label="Password" value={own} onChange={(e) => setOwn(e.currentTarget.value)} error={ownError} required autoComplete="new-password" data-testid="mailbox-password-input" />}
            <Group justify="flex-end" gap="xs">
              <Button variant="subtle" color="gray" onClick={close}>
                Cancel
              </Button>
              <Button type="submit" leftSection={<IconKey size={16} />} loading={set.isPending} disabled={mode === 'own' && (!own || !!ownError)} data-testid="mailbox-password-submit">
                Set password
              </Button>
            </Group>
          </Stack>
        </form>
      )}
    </Modal>
  );
}

function Endpoints({ on, list, name }: { on: boolean; list: MailEndpoint[]; name: string }) {
  if (!on) return <ToneBadge tone="neutral">{name} off</ToneBadge>;
  if (!list.length) return <ToneBadge tone="success">{name} on</ToneBadge>;
  return (
    <Group gap={6}>
      {list.map((e) => (
        <ToneBadge key={e.label} tone={e.tls ? 'info' : 'success'} testId={`mail-endpoint-${e.label.toLowerCase()}`}>
          {e.tls && <IconLock size={12} aria-hidden />}
          {endpointText(e)}
          {e.tls ? ' · TLS' : ''}
        </ToneBadge>
      ))}
    </Group>
  );
}

function LoginHelp({ wide }: { wide: boolean }) {
  const { features } = useMailFeature();
  const info = useMailServer();
  const imap = features.includes('mail.imap');
  const pop3 = features.includes('mail.pop3');
  const rule = loginRule(info, imap, pop3);
  return (
    <SectionCard title="Reading mail over IMAP and POP3" icon={<IconInbox size={16} />} description="Every recipient address gets a mailbox when its first message arrives" testId="mailbox-help">
      <Stack gap="xs">
        <DetailList
          columns={wide ? 1 : 2}
          items={[
            {
              label: 'Server',
              value: info.hostname ? (
                <span className="mono" data-testid="mailbox-help-host">
                  {info.hostname}
                </span>
              ) : (
                <span data-testid="mailbox-help-host">the FastMock mail Service or host</span>
              ),
            },
            { label: 'IMAP', value: <Endpoints on={imap} list={mailEndpoints(info, 'imap')} name="IMAP" /> },
            { label: 'POP3', value: <Endpoints on={pop3} list={mailEndpoints(info, 'pop3')} name="POP3" /> },
            { label: 'User name', value: 'the mailbox address, e.g. alice@example.test (plus tags removed)' },
            { label: 'Password', value: 'a managed FastMock token with mail:read in the mailbox’s space, or the mailbox password set here' },
            { label: 'Logins', value: <span data-testid="mailbox-help-logins">{rule}</span>, hidden: !rule },
          ]}
        />
        {!imap && !pop3 && (
          <Text size="xs" c="dimmed">
            Turn the listeners on with FASTMOCK_MAIL_IMAP_LISTEN, FASTMOCK_MAIL_IMAPS_LISTEN, FASTMOCK_MAIL_POP3_LISTEN or FASTMOCK_MAIL_POP3S_LISTEN (see the mail guide).
          </Text>
        )}
      </Stack>
    </SectionCard>
  );
}

/** An address that wraps only before the `@`; a part longer than the cell ends in an ellipsis. */
function Address({ address }: { address: string }) {
  const at = address.lastIndexOf('@');
  return (
    <Text size="sm" fw={600} className={`mono ${classes.address}`} title={address}>
      {at > 0 ? (
        <>
          {address.slice(0, at)}
          <wbr />
          {address.slice(at)}
        </>
      ) : (
        address
      )}
    </Text>
  );
}

export function MailMailboxes() {
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [editing, setEditing] = useState<MailboxView | null>(null);
  const [filter, setFilter] = useState('');
  const q = useQuery({ queryKey: ['mail', 'mailboxes', spaceQ.space ?? null], queryFn: () => mailApi.mailboxes(spaceQ.space) });
  const clear = useApiMutation<MailboxView>({
    fn: (b) => mailApi.clearPassword(b.address, b.space ?? null),
    invalidate: [['mail', 'mailboxes']],
    success: (_d, b) => `Password of ${b.address} removed`,
  });
  const wide = useMediaQuery('(min-width: 105em)');
  const boxes = (q.data?.mailboxes ?? []).filter((b) => !filter || b.address.includes(filter.trim().toLowerCase()));
  return (
    <Stack gap="md">
      <div className={classes.mailboxes}>
        <div className={classes.mailboxesHelp}>
          <LoginHelp wide={wide} />
        </div>
        <Stack gap="md" className={classes.mailboxesMain}>
          <TextInput placeholder="Filter by address" value={filter} onChange={(e) => setFilter(e.currentTarget.value)} aria-label="Filter mailboxes" />
          {q.isLoading ? (
            <LoadingState />
          ) : q.isError ? (
            <ErrorState error={q.error} onRetry={() => void q.refetch()} />
          ) : !q.data?.mailboxes.length ? (
            <EmptyState title="No mailboxes yet" icon={<IconMailbox size={28} />} docs={{ label: 'IMAP and POP3 in the mail guide', href: docsUrl('guide/mail') }} testId="mailboxes-empty">
              A mailbox appears for each recipient address when its first message arrives, or when you set its password through the API.
            </EmptyState>
          ) : (
            <DataTable testId="mailboxes-table" miw={560}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Address</Table.Th>
                  {spaces.enabled && <Table.Th>Space</Table.Th>}
                  <Table.Th>Folders</Table.Th>
                  <Table.Th>Password</Table.Th>
                  <Table.Th w={110}>
                    <span className="sr-only">Actions</span>
                  </Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {boxes.map((b) => {
                  const writable = can('mail:write', b.space ?? null);
                  return (
                    <Table.Tr key={`${b.space ?? ''}/${b.address}`} data-testid="mailbox-row">
                      <Table.Td className={classes.addressCell}>
                        <Address address={b.address} />
                      </Table.Td>
                      {spaces.enabled && (
                        <Table.Td data-label="Space" className={classes.badgeCell}>
                          <SpaceBadge space={b.space} />
                        </Table.Td>
                      )}
                      <Table.Td data-label="Folders">
                        <Group gap={4}>
                          {b.folders.map((f) => (
                            <span key={f} className="fm-chip">
                              {f}
                            </span>
                          ))}
                        </Group>
                      </Table.Td>
                      <Table.Td data-label="Password" className={classes.badgeCell}>
                        {b.hasPassword ? <ToneBadge tone="success">set</ToneBadge> : <ToneBadge tone="neutral">token only</ToneBadge>}
                      </Table.Td>
                      <Table.Td>
                        <Group gap={4} justify="flex-end" wrap="nowrap">
                          {writable ? (
                            <>
                              <Button size="compact-sm" variant="light" leftSection={<IconKey size={14} />} onClick={() => setEditing(b)} aria-label={`Set the password of ${b.address}`}>
                                {b.hasPassword ? 'Change' : 'Set'}
                              </Button>
                              {b.hasPassword && (
                                <Button
                                  size="compact-sm"
                                  variant="subtle"
                                  color="red"
                                  leftSection={<IconKeyOff size={14} />}
                                  aria-label={`Remove the password of ${b.address}`}
                                  onClick={() =>
                                    confirmAction({
                                      title: `Remove the password of ${b.address}?`,
                                      message: 'IMAP and POP3 logins then need a managed token with mail:read.',
                                      confirm: 'Remove password',
                                      onConfirm: () => clear.mutate(b),
                                    })
                                  }
                                >
                                  Remove
                                </Button>
                              )}
                            </>
                          ) : (
                            <Denied perm="mail:write" why={missingText('mail:write', b.space ?? null)}>
                              <Button size="compact-sm" variant="light" leftSection={<IconKey size={14} />}>
                                Set
                              </Button>
                            </Denied>
                          )}
                        </Group>
                      </Table.Td>
                    </Table.Tr>
                  );
                })}
              </Table.Tbody>
            </DataTable>
          )}
        </Stack>
      </div>
      <PasswordDialog box={editing} onClose={() => setEditing(null)} />
    </Stack>
  );
}
