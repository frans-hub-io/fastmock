import { Loader, Modal, Text, TextInput, UnstyledButton, useComputedColorScheme, useMantineColorScheme } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconArrowRight, IconCornerDownLeft, IconFlame, IconMail, IconMailPlus, IconMailbox, IconMoon, IconPlus, IconSearch, IconSun } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from 'react';
import { useNavigate } from 'react-router';
import { client, unwrap } from '../api/client';
import { useTokensApi, useVersion } from '../api/features';
import { mailApi } from '../api/mail';
import { can } from '../auth/permissions';
import { useAuth, useCan, useSpaceQuery } from '../auth/useAuth';
import { stubPath, stubTitle } from '../lib/stubs';
import { formatRelative } from '../lib/format';
import { senderOf } from '../lib/mail';
import { MethodBadge, PathText, StatusBadge } from './common';
import { visibleNav } from './nav';
import classes from './CommandPalette.module.css';

interface Entry {
  id: string;
  group: string;
  label: ReactNode;
  text: string;
  hint?: ReactNode;
  icon: ReactNode;
  run: () => void;
}

function matches(text: string, q: string): boolean {
  if (!q) return true;
  const t = text.toLowerCase();
  return q
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => t.includes(w));
}

/**
 * The command palette (Ctrl/Cmd+K): jump to a screen, a stub by name or path, or a recent request. Only what the
 * principal may read is offered, and stubs and requests are fetched only while it is open.
 */
export function CommandPalette({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const navigate = useNavigate();
  const { permissions } = useAuth();
  const canDo = useCan();
  const spaceQ = useSpaceQuery();
  const tokensApi = useTokensApi() === 'available';
  const { setColorScheme } = useMantineColorScheme({ keepTransitions: true });
  const scheme = useComputedColorScheme('light');
  const [q, setQ] = useState('');
  const [debounced] = useDebouncedValue(q.trim(), 200);
  const [active, setActive] = useState(0);
  const listId = useId();
  const listRef = useRef<HTMLDivElement>(null);
  const readStubs = can(permissions, 'stubs:read');
  const readJournal = can(permissions, 'journal:read');
  const features = useVersion().data?.features;
  const mailOn = !!features?.includes('mail');
  const readMail = mailOn && can(permissions, 'mail:read');

  const stubs = useQuery({
    queryKey: ['palette', 'stubs', debounced, spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/stubs', { params: { query: { q: debounced || undefined, limit: 6, ...spaceQ } } })),
    enabled: opened && readStubs,
    staleTime: 5_000,
  });
  const requests = useQuery({
    queryKey: ['palette', 'requests', spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/requests', { params: { query: { limit: 50, ...spaceQ } } })),
    enabled: opened && readJournal,
    staleTime: 5_000,
  });

  const mail = useQuery({
    queryKey: ['palette', 'mail', debounced, spaceQ],
    queryFn: () => mailApi.list({ ...spaceQ, query: debounced || undefined, limit: 5 }),
    enabled: opened && readMail,
    staleTime: 5_000,
    retry: false,
  });

  useEffect(() => {
    if (!opened) {
      setQ('');
      setActive(0);
    }
  }, [opened]);

  const go = (to: string, state?: unknown) => {
    onClose();
    void navigate(to, state === undefined ? undefined : { state });
  };

  const entries = useMemo<Entry[]>(() => {
    const term = q.trim();
    const out: Entry[] = [];
    for (const g of visibleNav(permissions, tokensApi, features)) {
      for (const i of g.items) {
        const text = `${i.label} ${g.group} ${i.keywords ?? ''}`;
        if (!matches(text, term)) continue;
        out.push({ id: `page:${i.to}`, group: 'Pages', label: i.label, text, hint: g.group, icon: <i.icon size={18} stroke={1.6} />, run: () => go(i.to) });
      }
    }
    const actions: Entry[] = [];
    if (canDo('stubs:write')) actions.push({ id: 'act:new-stub', group: 'Actions', label: 'Create a stub', text: 'create new stub add', icon: <IconPlus size={18} />, run: () => go('/stubs/new') });
    if (readMail) {
      if (canDo('mail:write'))
        actions.push({ id: 'act:inject-mail', group: 'Actions', label: 'Inject a mail message', text: 'inject mail email message compose send eml', icon: <IconMailPlus size={18} />, run: () => go('/mail?inject=1') });
      actions.push({ id: 'act:mailboxes', group: 'Actions', label: 'Mailboxes and IMAP/POP3 passwords', text: 'mailboxes imap pop3 password mail', icon: <IconMailbox size={18} />, run: () => go('/mail/mailboxes') });
      actions.push({ id: 'act:mail-chaos', group: 'Actions', label: 'Mail chaos rules', text: 'mail chaos smtp faults reply delay disconnect', icon: <IconFlame size={18} />, run: () => go('/mail/chaos') });
    }
    const next = scheme === 'dark' ? 'light' : 'dark';
    actions.push({
      id: 'act:theme',
      group: 'Actions',
      label: `Switch to ${next} theme`,
      text: `theme dark light mode switch ${next}`,
      icon: next === 'dark' ? <IconMoon size={18} /> : <IconSun size={18} />,
      run: () => {
        setColorScheme(next);
        onClose();
      },
    });
    out.push(...actions.filter((a) => matches(a.text, term)));
    for (const s of stubs.data?.stubs ?? []) {
      out.push({
        id: `stub:${s.id ?? ''}`,
        group: 'Stubs',
        label: stubTitle(s),
        text: `${stubTitle(s)} ${stubPath(s)}`,
        hint: <PathText path={stubPath(s)} />,
        icon: <MethodBadge method={s.request.method} />,
        run: () => go(`/stubs/${s.id ?? ''}`),
      });
    }
    const seen = new Set<string>();
    const reqs = (requests.data?.requests ?? [])
      .filter((r) => {
        const key = `${r.method} ${r.path}?${r.query}`;
        if (seen.has(key) || !matches(`${r.method} ${r.path} ${String(r.status)}`, term)) return false;
        seen.add(key);
        return true;
      })
      .slice(0, 6);
    for (const r of reqs) {
      out.push({
        id: `req:${String(r.id)}`,
        group: 'Recent requests',
        label: <PathText path={r.path} />,
        text: r.path,
        hint: (
          <span className={classes.reqHint}>
            <StatusBadge status={r.status} />
            <span>{formatRelative(r.at)}</span>
          </span>
        ),
        icon: <MethodBadge method={r.method} />,
        run: () => go(`/requests?id=${String(r.id)}`),
      });
    }
    for (const m of mail.data?.messages ?? []) {
      const subject = m.summary.subject || '(no subject)';
      out.push({
        id: `mail:${m.id}`,
        group: 'Recent mail',
        label: subject,
        text: `${subject} ${senderOf(m)}`,
        hint: (
          <span className={classes.reqHint}>
            <span>{senderOf(m)}</span>
            <span>{formatRelative(m.receivedAt)}</span>
          </span>
        ),
        icon: <IconMail size={18} />,
        run: () => go(`/mail/${m.id}`),
      });
    }
    return out;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [q, permissions, tokensApi, features, stubs.data, requests.data, mail.data, scheme]);

  useEffect(() => {
    setActive((a) => (a >= entries.length ? 0 : a));
  }, [entries.length]);

  useEffect(() => {
    listRef.current?.querySelector('[data-active="true"]')?.scrollIntoView({ block: 'nearest' });
  }, [active]);

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      setActive((a) => (entries.length ? (a + 1) % entries.length : 0));
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      setActive((a) => (entries.length ? (a - 1 + entries.length) % entries.length : 0));
    } else if (e.key === 'Enter') {
      e.preventDefault();
      entries[active]?.run();
    }
  };

  const loading = (stubs.isFetching && !stubs.data) || (requests.isFetching && !requests.data) || (mail.isFetching && !mail.data);
  let lastGroup = '';

  return (
    <Modal
      opened={opened}
      onClose={onClose}
      withCloseButton={false}
      padding={0}
      size={640}
      yOffset="12vh"
      title={<span className="sr-only">Search</span>}
      classNames={{ header: 'sr-only', content: classes.content }}
      data-testid="command-palette"
    >
      <div className={classes.inputRow}>
        <TextInput
          data-autofocus
          variant="unstyled"
          size="md"
          value={q}
          onChange={(e) => {
            setQ(e.currentTarget.value);
            setActive(0);
          }}
          onKeyDown={onKeyDown}
          placeholder="Search pages, stubs and recent requests…"
          leftSection={loading ? <Loader size={16} /> : <IconSearch size={18} />}
          classNames={{ input: classes.input }}
          role="combobox"
          aria-label="Search pages, stubs and recent requests"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={entries[active] ? `${listId}-${String(active)}` : undefined}
          aria-autocomplete="list"
          data-testid="palette-input"
        />
        <span className="fm-kbd">Esc</span>
      </div>
      <div className={classes.list} id={listId} role="listbox" aria-label="Results" ref={listRef}>
        {entries.length === 0 && (
          <Text size="sm" c="dimmed" ta="center" py="xl">
            Nothing matches “{q}”.
          </Text>
        )}
        {entries.map((en, i) => {
          const heading = en.group !== lastGroup ? en.group : null;
          lastGroup = en.group;
          return (
            <div key={en.id}>
              {heading && (
                <div className={classes.group} role="presentation">
                  {heading}
                </div>
              )}
              <UnstyledButton
                id={`${listId}-${String(i)}`}
                role="option"
                aria-selected={i === active}
                data-active={i === active}
                className={classes.item}
                onMouseMove={() => i !== active && setActive(i)}
                onClick={() => en.run()}
                tabIndex={-1}
                data-testid="palette-item"
              >
                <span className={classes.itemIcon}>{en.icon}</span>
                <span className={classes.itemText}>
                  <span className={classes.itemLabel}>{en.label}</span>
                  {en.hint && <span className={classes.itemHint}>{en.hint}</span>}
                </span>
                <span className={classes.itemGo} aria-hidden>
                  {i === active ? <IconCornerDownLeft size={16} /> : <IconArrowRight size={14} />}
                </span>
              </UnstyledButton>
            </div>
          );
        })}
      </div>
      <div className={classes.footer}>
        <span>
          <span className="fm-kbd">↑</span> <span className="fm-kbd">↓</span> to move
        </span>
        <span>
          <span className="fm-kbd">Enter</span> to open
        </span>
      </div>
    </Modal>
  );
}
