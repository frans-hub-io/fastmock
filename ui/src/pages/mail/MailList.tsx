import { ActionIcon, Button, Group, Paper, Popover, Stack, Table, Tabs, Text, TextInput, Tooltip, UnstyledButton } from '@mantine/core';
import { IconChecks, IconHelpCircle, IconMail, IconMailPlus, IconPaperclip, IconRefresh, IconSearch, IconTrash, IconX } from '@tabler/icons-react';
import { useInfiniteQuery, useQueryClient, type InfiniteData } from '@tanstack/react-query';
import { useEffect, useMemo, useState } from 'react';
import { Link, useSearchParams } from 'react-router';
import { mailApi, useMailServer, type MailEntry, type MailList as MailListPage } from '../../api/mail';
import { confirmAction, useApiMutation } from '../../api/mutations';
import { useCan, useSpace, useSpaceQuery } from '../../auth/useAuth';
import { Permitted } from '../../components/Permitted';
import { SpaceBadge } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { docsUrl } from '../../lib/docs';
import { formatAbsolute, formatRelative } from '../../lib/format';
import { endpointText, isUnread, loadSeen, mailEndpoints, markAllSeen, QUERY_HELP, recipientsOf, senderOf, smtpSnippets, type SeenState } from '../../lib/mail';
import { useMailStream, type StreamEvent } from '../../lib/mailStream';
import { InjectDialog } from './MailInject';
import classes from './Mail.module.css';
import { LabelChip, LiveStatus, Snippet, SourceBadge } from './parts';

const PAGE = 50;

function QueryHelp({ onPick }: { onPick: (term: string) => void }) {
  const [open, setOpen] = useState(false);
  return (
    <Popover opened={open} onChange={setOpen} position="bottom-end" width={420} withArrow shadow="md">
      <Popover.Target>
        <ActionIcon variant="subtle" color="gray" onClick={() => setOpen((o) => !o)} aria-label="Search syntax" data-testid="mail-query-help">
          <IconHelpCircle size={18} />
        </ActionIcon>
      </Popover.Target>
      <Popover.Dropdown>
        <Text size="sm" fw={700} mb={4}>
          Search syntax
        </Text>
        <Text size="xs" c="dimmed" mb="xs">
          All terms must hold. Click a term to add it.
        </Text>
        <Table fz="xs" verticalSpacing={4} withRowBorders={false}>
          <Table.Tbody>
            {QUERY_HELP.map((h) => (
              <Table.Tr key={h.term}>
                <Table.Td pl={0}>
                  <UnstyledButton
                    className={`fm-chip ${classes.helpTerm}`}
                    onClick={() => {
                      onPick(h.term);
                      setOpen(false);
                    }}
                  >
                    {h.term}
                  </UnstyledButton>
                </Table.Td>
                <Table.Td c="dimmed">{h.meaning}</Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      </Popover.Dropdown>
    </Popover>
  );
}

function MailRow({ entry, unread, fresh, showSpace }: { entry: MailEntry; unread: boolean; fresh: boolean; showSpace: boolean }) {
  const subject = entry.summary.subject || '(no subject)';
  const preview = entry.summary.preview ?? '';
  return (
    <li>
      <Link
        to={`/mail/${entry.id}`}
        className={classes.row}
        data-unread={unread || undefined}
        data-fresh={fresh || undefined}
        data-testid="mail-row"
        aria-label={`${unread ? 'Unread. ' : ''}${senderOf(entry)}: ${subject}`}
      >
        <span className={classes.dot} aria-hidden />
        <span className={classes.from} title={`${senderOf(entry)} → ${recipientsOf(entry)}`}>
          {senderOf(entry)}
        </span>
        <span className={classes.body}>
          <span className={classes.subject}>{subject}</span>
          {preview && (
            <span className={classes.preview}>
              <span className={classes.sep}> — </span>
              {preview}
            </span>
          )}
        </span>
        <span className={classes.tags}>
          {entry.summary.hasAttachments && (
            <Tooltip label={(entry.summary.attachmentNames ?? []).join(', ') || 'Attachments'} withArrow>
              <IconPaperclip size={15} className={classes.clip} aria-label="Has attachments" data-testid="mail-attachment-icon" />
            </Tooltip>
          )}
          {entry.labels.slice(0, 3).map((l) => (
            <LabelChip key={l} label={l} />
          ))}
          {entry.labels.length > 3 && <span className="fm-chip">+{entry.labels.length - 3}</span>}
          {showSpace && entry.space && <SpaceBadge space={entry.space} />}
          <SourceBadge entry={entry} />
        </span>
        <time className={classes.time} dateTime={entry.receivedAt} title={formatAbsolute(entry.receivedAt)}>
          {formatRelative(entry.receivedAt)}
        </time>
      </Link>
    </li>
  );
}

function FirstMessage({ live }: { live: boolean }) {
  const info = useMailServer();
  const eps = mailEndpoints(info, 'smtp');
  const ep = eps[0];
  const smtpOff = info.published && !ep;
  const host = info.hostname ?? (window.location.hostname || 'localhost');
  const snippets = smtpSnippets(host, ep?.port ?? 2525, ep?.tls ?? false);
  const ports = eps.length ? eps.map((e) => `${endpointText(e)}${e.tls ? ' (TLS)' : ''}`).join(', ') : 'port 2525, the default';
  const arrive = `Messages appear here ${live ? 'the moment they arrive' : 'when you reload'}.`;
  return (
    <EmptyState title="No mail yet" icon={<IconMail size={30} />} docs={{ label: 'Mail guide: SMTP, API, IMAP and POP3', href: docsUrl('guide/mail') }} testId="mail-empty">
      <Stack gap="sm" align="center">
        {smtpOff ? (
          <span data-testid="mail-empty-smtp">The SMTP listeners are off (FASTMOCK_MAIL_SMTP_LISTEN, FASTMOCK_MAIL_SMTPS_LISTEN): inject a message instead. {arrive}</span>
        ) : (
          <>
            <span data-testid="mail-empty-smtp">
              Point your application at FastMock's SMTP listener on {info.hostname ? <span className="mono">{info.hostname}</span> : 'the FastMock mail Service or host'}, {ports}. {arrive} Send a test message:
            </span>
            {!info.hostname && (
              <Text size="xs" c="dimmed">
                The snippets use this page’s host, {host}; replace it with the mail Service or host when they differ.
              </Text>
            )}
            <Tabs defaultValue="swaks" variant="pills" radius="md" className={classes.emptyCard} classNames={{ list: 'fm-tabs-list' }}>
              <Tabs.List>
                {snippets.map((s) => (
                  <Tabs.Tab key={s.id} value={s.id}>
                    {s.label}
                  </Tabs.Tab>
                ))}
              </Tabs.List>
              {snippets.map((s) => (
                <Tabs.Panel key={s.id} value={s.id} pt="xs">
                  <Snippet code={s.code} testId={`smtp-snippet-${s.id}`} />
                </Tabs.Panel>
              ))}
            </Tabs>
          </>
        )}
      </Stack>
    </EmptyState>
  );
}

export function MailList() {
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const qc = useQueryClient();
  const [params, setParams] = useSearchParams();
  const query = params.get('q') ?? '';
  const [text, setText] = useState(query);
  const [seen, setSeen] = useState<SeenState>(loadSeen);
  const [freshIds, setFreshIds] = useState<Set<string>>(new Set());
  const [pending, setPending] = useState(0);
  const injectOpen = params.get('inject') === '1';

  useEffect(() => setText(query), [query]);
  useEffect(() => {
    setPending(0);
  }, [query, spaceQ.space]);

  const setParam = (k: string, v: string | null) => {
    const next = new URLSearchParams(params);
    if (!v) next.delete(k);
    else next.set(k, v);
    setParams(next, { replace: true });
  };
  const runSearch = (q: string) => setParam('q', q.trim() || null);

  const listKey = ['mail', 'list', spaceQ.space ?? null, query];
  const list = useInfiniteQuery({
    queryKey: listKey,
    queryFn: ({ pageParam }) => mailApi.list({ ...spaceQ, query: query || undefined, cursor: pageParam, limit: PAGE }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
  });

  /** Removes deleted messages from every cached list, so a list opened later does not show them either. */
  const dropFromLists = (ids: readonly string[]) => {
    const gone = new Set(ids);
    qc.setQueriesData<InfiniteData<MailListPage>>({ queryKey: ['mail', 'list'] }, (old) =>
      old ? { ...old, pages: old.pages.map((pg) => ({ ...pg, messages: pg.messages.filter((m) => !gone.has(m.id)) })) } : old,
    );
  };

  const onEvent = (e: StreamEvent) => {
    if (e.type === 'resync') {
      setPending(0);
      void qc.invalidateQueries({ queryKey: ['mail', 'list'] });
      return;
    }
    if (e.type === 'message.deleted') {
      dropFromLists(e.event.ids);
      return;
    }
    const entry = e.event.entry;
    if (!entry) {
      setPending((n) => n + 1);
      return;
    }
    if (query) {
      setPending((n) => n + 1);
      return;
    }
    qc.setQueryData<InfiniteData<MailListPage>>(listKey, (old) => {
      const [first, ...rest] = old?.pages ?? [];
      if (!old || !first || old.pages.some((pg) => pg.messages.some((m) => m.id === entry.id))) return old;
      return { ...old, pages: [{ ...first, messages: [entry, ...first.messages] }, ...rest] };
    });
    setFreshIds((s) => new Set([...s, entry.id]));
  };
  const status = useMailStream(spaceQ.space, true, onEvent);

  const items = useMemo(() => {
    const seenIds = new Set<string>();
    const out: MailEntry[] = [];
    for (const e of list.data?.pages.flatMap((p) => p.messages) ?? []) {
      if (seenIds.has(e.id)) continue;
      seenIds.add(e.id);
      out.push(e);
    }
    return out;
  }, [list.data]);

  const unread = items.filter((e) => isUnread(seen, e)).length;
  const canDelete = can('mail:delete');

  const clear = useApiMutation<void, { deleted: number; ids: string[] }>({
    fn: () => mailApi.removeMatching({ ...spaceQ, query: query || undefined }),
    invalidate: [['mail', 'list']],
    success: (d) => `Deleted ${String(d.deleted)} message${d.deleted === 1 ? '' : 's'}`,
    onSuccess: (d) => dropFromLists(d.ids),
  });

  const where = spaces.current ? ` in space ${spaces.current}` : spaces.enabled ? ' in every space you may delete in' : '';

  return (
    <Stack gap="md">
      <div className="fm-toolbar">
        <form
          className="fm-grow"
          onSubmit={(e) => {
            e.preventDefault();
            runSearch(text);
          }}
          role="search"
        >
          <TextInput
            placeholder='Search: from:billing subject:"invoice" has:attachment label:otp'
            leftSection={<IconSearch size={16} />}
            rightSectionWidth={text ? 64 : 36}
            rightSection={
              <Group gap={0} wrap="nowrap">
                {text && (
                  <ActionIcon
                    variant="subtle"
                    color="gray"
                    aria-label="Clear search"
                    onClick={() => {
                      setText('');
                      runSearch('');
                    }}
                  >
                    <IconX size={16} />
                  </ActionIcon>
                )}
                <QueryHelp
                  onPick={(t) => {
                    const next = text.trim() ? `${text.trim()} ${t}` : t;
                    setText(next);
                    runSearch(next);
                  }}
                />
              </Group>
            }
            value={text}
            onChange={(e) => setText(e.currentTarget.value)}
            onBlur={() => text !== query && runSearch(text)}
            aria-label="Search mail"
            classNames={{ input: 'mono' }}
            data-testid="mail-search"
          />
        </form>
        <div className={classes.actions}>
          <Tooltip label="Mark all as read">
            <ActionIcon variant="default" size="lg" onClick={() => setSeen(markAllSeen())} aria-label="Mark all as read" disabled={!unread} data-testid="mail-mark-read">
              <IconChecks size={18} />
            </ActionIcon>
          </Tooltip>
          <Tooltip label="Reload">
            <ActionIcon variant="default" size="lg" onClick={() => void list.refetch()} aria-label="Reload">
              <IconRefresh size={18} />
            </ActionIcon>
          </Tooltip>
          {canDelete && items.length > 0 && (
            <Button
              variant="subtle"
              color="red"
              leftSection={<IconTrash size={16} />}
              data-testid="mail-delete-matching"
              onClick={() =>
                confirmAction({
                  title: query ? 'Delete the matching messages?' : 'Delete all messages?',
                  message: query ? (
                    <Text size="sm">
                      Every message matching <code>{query}</code>
                      {where} is deleted, also from IMAP and POP3 mailboxes. This cannot be undone.
                    </Text>
                  ) : (
                    `Every captured message${where} is deleted, also from IMAP and POP3 mailboxes. This cannot be undone.`
                  ),
                  confirm: query ? 'Delete matching' : 'Delete all',
                  onConfirm: () => clear.mutate(),
                })
              }
            >
              {query ? 'Delete matching' : 'Delete all'}
            </Button>
          )}
          <Permitted perm="mail:write" hide>
            <Button leftSection={<IconMailPlus size={16} />} onClick={() => setParam('inject', '1')} data-testid="mail-inject">
              Inject
            </Button>
          </Permitted>
        </div>
      </div>

      {pending > 0 && (
        <div className={classes.banner} role="status" data-testid="mail-pending">
          <span>
            {pending} new message{pending === 1 ? '' : 's'} arrived{query ? ' and may match the search' : ''}.
          </span>
          <Button
            size="compact-sm"
            variant="light"
            onClick={() => {
              setPending(0);
              void list.refetch();
            }}
          >
            Show
          </Button>
        </div>
      )}

      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !items.length ? (
        query ? (
          <EmptyState title="No messages match the search" icon={<IconSearch size={28} />} action={<Button variant="light" onClick={() => runSearch('')}>Clear search</Button>}>
            Nothing matches <code>{query}</code>
            {spaces.current ? ` in space ${spaces.current}` : ''}. New matching messages appear as they arrive.
          </EmptyState>
        ) : (
          <FirstMessage live={status === 'live'} />
        )
      ) : (
        <Paper withBorder radius="md" className="fm-table-card">
          <ul className={classes.list} aria-label="Messages" data-testid="mail-list">
            {items.map((e) => (
              <MailRow key={e.id} entry={e} unread={isUnread(seen, e)} fresh={freshIds.has(e.id)} showSpace={spaces.enabled && !spaces.current} />
            ))}
          </ul>
          <div className={classes.listFoot}>
            <Group gap="sm">
              <LiveStatus status={status} />
              <Text size="xs" c="dimmed">
                {items.length} message{items.length === 1 ? '' : 's'}
                {unread ? ` · ${String(unread)} unread` : ''}
              </Text>
            </Group>
            {list.hasNextPage && (
              <Button size="compact-sm" variant="light" loading={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()} data-testid="mail-load-more">
                Load more
              </Button>
            )}
          </div>
        </Paper>
      )}
      {!list.isLoading && !items.length && (
        <Group justify="center">
          <LiveStatus status={status} />
        </Group>
      )}
      <InjectDialog opened={injectOpen} onClose={() => setParam('inject', null)} />
    </Stack>
  );
}
