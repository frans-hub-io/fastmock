import {
  ActionIcon,
  Badge,
  Burger,
  Drawer,
  Group,
  Menu,
  NavLink,
  ScrollArea,
  Text,
  Tooltip,
  UnstyledButton,
  useComputedColorScheme,
  useMantineColorScheme,
} from '@mantine/core';
import { useDisclosure } from '@mantine/hooks';
import {
  IconActivity,
  IconApi,
  IconBolt,
  IconBox,
  IconBroadcast,
  IconChecklist,
  IconDatabase,
  IconFileCode,
  IconFiles,
  IconFlame,
  IconGauge,
  IconHistory,
  IconKey,
  IconLogout,
  IconMoon,
  IconPlayerRecord,
  IconRoute,
  IconSearch,
  IconSettings,
  IconShieldCheck,
  IconSitemap,
  IconSun,
  IconUser,
  IconUserShield,
  IconWebhook,
} from '@tabler/icons-react';
import type { ComponentType, ReactNode } from 'react';
import { Link, NavLink as RouterNavLink, useLocation } from 'react-router';
import lockupDark from '../assets/brand/FransHub_FastMock_dark.svg';
import lockupLight from '../assets/brand/FransHub_FastMock_light.svg';
import monogramDark from '../assets/brand/FransHub_monogram_dark.svg';
import monogramLight from '../assets/brand/FransHub_monogram_light.svg';
import { useTokensApi } from '../api/features';
import { boundSpaces, can, kindLabel, seesAllSpaces, spaceScoped, type Permission } from '../auth/permissions';
import { useAuth, useSpace } from '../auth/useAuth';
import classes from './Shell.module.css';
import { SpaceSelect } from './spaces';
import { NoAccessState } from './States';

interface NavItem {
  to: string;
  label: string;
  icon: ComponentType<{ size?: number; stroke?: number }>;
  perm?: Permission;
  /** The permission must be held globally (the screen is not scoped by space). */
  global?: boolean;
  /** Shown only when the server offers the managed-token API. */
  tokensApi?: boolean;
}

const NAV: { group: string; items: NavItem[] }[] = [
  { group: 'Overview', items: [{ to: '/', label: 'Dashboard', icon: IconGauge }] },
  {
    group: 'Mocks',
    items: [
      { to: '/stubs', label: 'Stubs', icon: IconRoute, perm: 'stubs:read' },
      { to: '/services', label: 'Services', icon: IconSitemap, perm: 'stubs:read', global: true },
      { to: '/scenarios', label: 'Scenarios', icon: IconChecklist, perm: 'state:read' },
      { to: '/async', label: 'Async mocks', icon: IconBroadcast, perm: 'stubs:read', global: true },
      { to: '/grpc', label: 'gRPC', icon: IconFileCode, perm: 'grpc:read' },
      { to: '/files', label: 'Files', icon: IconFiles, perm: 'files:read' },
    ],
  },
  {
    group: 'Traffic',
    items: [
      { to: '/requests', label: 'Request journal', icon: IconActivity, perm: 'journal:read' },
      { to: '/near-misses', label: 'Near misses', icon: IconSearch, perm: 'journal:read' },
      { to: '/verify', label: 'Verify', icon: IconShieldCheck, perm: 'journal:read' },
      { to: '/callbacks', label: 'Callbacks', icon: IconWebhook, perm: 'callbacks:read' },
      { to: '/recordings', label: 'Recordings', icon: IconPlayerRecord, perm: 'recordings:read' },
    ],
  },
  {
    group: 'Testing',
    items: [
      { to: '/contract-tests', label: 'Contract tests', icon: IconBox, perm: 'contracts:read' },
      { to: '/chaos', label: 'Chaos', icon: IconFlame, perm: 'chaos:read' },
    ],
  },
  {
    group: 'System',
    items: [
      { to: '/kv', label: 'KV store', icon: IconDatabase, perm: 'state:read' },
      { to: '/settings', label: 'Settings', icon: IconSettings, perm: 'chaos:read' },
      { to: '/tokens', label: 'Admin tokens', icon: IconKey, perm: 'tokens:read', tokensApi: true },
      { to: '/access', label: 'Access', icon: IconUserShield },
      { to: '/audit', label: 'Audit log', icon: IconHistory, perm: 'audit:read' },
      { to: '/api', label: 'API docs', icon: IconApi },
    ],
  },
];

const ROLE_COLOR: Record<string, string> = { viewer: 'gray', editor: 'blue', admin: 'grape' };

/** The navigation entry a path belongs to: the deepest entry that is the path or one of its parents. */
function navItemOf(pathname: string): NavItem | undefined {
  return NAV.flatMap((g) => g.items)
    .filter((i) => i.to !== '/' && (pathname === i.to || pathname.startsWith(`${i.to}/`)))
    .sort((a, b) => b.to.length - a.to.length)[0];
}

/** Shows the no-access state instead of a screen whose read permission the principal lacks (opened by URL). */
function PageGuard({ children }: { children: ReactNode }) {
  const { permissions } = useAuth();
  const loc = useLocation();
  const item = navItemOf(loc.pathname);
  if (permissions && item?.perm && !can(permissions, item.perm, item.global ? null : undefined)) {
    return <NoAccessState perm={item.perm} global={item.global || !spaceScoped(item.perm)} />;
  }
  return <>{children}</>;
}

function Nav({ onNavigate }: { onNavigate?: () => void }) {
  const { permissions } = useAuth();
  const tokensApi = useTokensApi() === 'available';
  const loc = useLocation();
  const shown = (i: NavItem) => (!i.perm || can(permissions, i.perm, i.global ? null : undefined)) && (!i.tokensApi || tokensApi);
  const groups = NAV.map((g) => ({ ...g, items: g.items.filter(shown) })).filter((g) => g.items.length > 0);
  return (
    <nav aria-label="Main">
      {groups.map((g) => (
        <div key={g.group}>
          <div className={classes.group}>{g.group}</div>
          {g.items.map((i) => {
            const active = i.to === '/' ? loc.pathname === '/' : loc.pathname === i.to || loc.pathname.startsWith(`${i.to}/`);
            return (
              <NavLink
                key={i.to}
                component={RouterNavLink}
                to={i.to}
                label={i.label}
                leftSection={<i.icon size={18} stroke={1.6} />}
                active={active}
                onClick={onNavigate}
                variant="light"
                className={classes.navLink}
              />
            );
          })}
        </div>
      ))}
    </nav>
  );
}

function ThemeToggle() {
  const { setColorScheme } = useMantineColorScheme({ keepTransitions: true });
  const computed = useComputedColorScheme('light');
  const next = computed === 'dark' ? 'light' : 'dark';
  return (
    <Tooltip label={`Switch to ${next} theme`} withArrow>
      <ActionIcon variant="default" size="lg" onClick={() => setColorScheme(next)} aria-label={`Switch to ${next} theme`}>
        {computed === 'dark' ? <IconSun size={18} /> : <IconMoon size={18} />}
      </ActionIcon>
    </Tooltip>
  );
}

/** The role badge: the role for principals that hold it everywhere, else the spaces they are bound to. */
function scopeLabel(role: string, spaces: string[], global: boolean, allSpaces: boolean): string {
  if (global || !spaces.length) return role;
  if (allSpaces) return 'all spaces';
  return spaces.length === 1 ? (spaces[0] ?? role) : `${spaces.length} spaces`;
}

function Principal() {
  const { name, role, kind, info, permissions, logout } = useAuth();
  const spaces = boundSpaces(permissions);
  const global = seesAllSpaces(permissions);
  const allSpaces = !!permissions?.allSpaces?.length;
  const badge = scopeLabel(role, spaces, global, allSpaces);
  const groups = info?.principal.groups ?? [];
  const who = name || 'anonymous';
  return (
    <Menu position="bottom-end" withArrow width={280}>
      <Menu.Target>
        <UnstyledButton aria-label={`Account: ${who}`} data-testid="principal" className={classes.principal}>
          <Group gap={8} wrap="nowrap">
            <IconUser size={18} aria-hidden />
            <Text size="sm" fw={500} visibleFrom="sm" className="truncate" maw={160} data-testid="principal-name">
              {who}
            </Text>
            <span className={classes.kind}>
              <Badge color="gray" variant="outline" data-testid="kind-badge" tt="none">
                {kindLabel(kind)}
              </Badge>
            </span>
            <Badge color={ROLE_COLOR[role] ?? 'teal'} variant="light" data-testid="role-badge" className={classes.roleBadge} tt={badge === role ? undefined : 'none'}>
              {badge}
            </Badge>
          </Group>
        </UnstyledButton>
      </Menu.Target>
      <Menu.Dropdown>
        <Menu.Label>Signed in as</Menu.Label>
        <div className={classes.who}>
          <Text size="sm" fw={600} className="truncate" title={who}>
            {who}
          </Text>
          <Text size="xs" c="dimmed">
            {kindLabel(kind)}
            {global || !spaces.length ? ` · ${role}` : ''}
          </Text>
          {spaces.length > 0 && (
            <Text size="xs" c="dimmed" data-testid="principal-spaces">
              Spaces: {allSpaces ? `all (${spaces.join(', ')})` : spaces.join(', ')}
            </Text>
          )}
          {groups.length > 0 && (
            <Text size="xs" c="dimmed" lineClamp={3}>
              Groups: {groups.join(', ')}
            </Text>
          )}
        </div>
        {info?.authDisabled && <Menu.Label c="orange">Authentication is disabled on this server</Menu.Label>}
        <Menu.Divider />
        <Menu.Item leftSection={<IconBolt size={16} />} component={Link} to="/">
          Dashboard
        </Menu.Item>
        <Menu.Item leftSection={<IconUserShield size={16} />} component={Link} to="/access">
          Your access
        </Menu.Item>
        {!info?.authDisabled && (
          <Menu.Item leftSection={<IconLogout size={16} />} color="red" onClick={() => logout()} data-testid="logout">
            Sign out
          </Menu.Item>
        )}
      </Menu.Dropdown>
    </Menu>
  );
}

export function Shell({ children }: { children: ReactNode }) {
  const [opened, { toggle, close }] = useDisclosure(false);
  const { info } = useAuth();
  const spaces = useSpace();
  return (
    <div className={classes.root}>
      <a href="#main" className={classes.skip}>
        Skip to content
      </a>
      <header className={classes.header}>
        <Group gap="sm" wrap="nowrap" className={classes.headerLeft}>
          <Burger opened={opened} onClick={toggle} hiddenFrom="md" size="sm" aria-label="Toggle navigation" />
          <Link to="/" className={`${classes.brand} ${spaces.enabled ? classes.brandCompact : ''}`} aria-label="FastMock">
            <span className={`${classes.lockup} ${spaces.enabled ? classes.lockupCompact : ''}`}>
              <img src={lockupLight} alt="" width={193} height={26} className="fm-on-light" />
              <img src={lockupDark} alt="" width={193} height={26} className="fm-on-dark" />
            </span>
            <span className={`${classes.mark} ${spaces.enabled ? classes.markCompact : ''}`}>
              <img src={monogramLight} alt="" width={42} height={24} className="fm-on-light" />
              <img src={monogramDark} alt="" width={42} height={24} className="fm-on-dark" />
            </span>
          </Link>
          {info?.version && (
            <Badge variant="outline" color="gray" size="sm" visibleFrom="sm">
              v{info.version}
            </Badge>
          )}
          <div className={classes.spaceSlot}>
            <SpaceSelect />
          </div>
        </Group>
        <Group gap="xs" wrap="nowrap" className={classes.headerRight}>
          <ThemeToggle />
          <Principal />
        </Group>
      </header>
      <div className={classes.body}>
        <div className={classes.sideCol}>
          <aside className={classes.sidebar}>
            <Nav />
          </aside>
        </div>
        <Drawer opened={opened} onClose={close} size={280} title="FastMock" hiddenFrom="md">
          <ScrollArea>
            <Nav onNavigate={close} />
          </ScrollArea>
        </Drawer>
        <main id="main" className={classes.main} tabIndex={-1}>
          <PageGuard>{children}</PageGuard>
        </main>
      </div>
    </div>
  );
}
