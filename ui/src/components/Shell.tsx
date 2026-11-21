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
import { useDisclosure, useHotkeys } from '@mantine/hooks';
import {
  IconBolt,
  IconChevronDown,
  IconLayoutSidebarLeftCollapse,
  IconLayoutSidebarLeftExpand,
  IconLogout,
  IconMoon,
  IconSearch,
  IconSun,
  IconUserShield,
} from '@tabler/icons-react';
import { useState, type ReactNode } from 'react';
import { Link, NavLink as RouterNavLink, useLocation } from 'react-router';
import lockupDark from '../assets/brand/FransHub_FastMock_dark.svg';
import lockupLight from '../assets/brand/FransHub_FastMock_light.svg';
import monogramDark from '../assets/brand/FransHub_monogram_dark.svg';
import monogramLight from '../assets/brand/FransHub_monogram_light.svg';
import { useTokensApi, useVersion } from '../api/features';
import { accessSummary, can, kindLabel, spaceScoped, summaryLabel } from '../auth/permissions';
import { useAuth, useSpace } from '../auth/useAuth';
import { readLocal, writeLocal } from '../lib/ui';
import { CommandPalette } from './CommandPalette';
import { isActive, navItemOf, visibleNav } from './nav';
import classes from './Shell.module.css';
import { SpaceSelect } from './spaces';
import { NoAccessState } from './States';

const ROLE_COLOR: Record<string, string> = { viewer: 'gray', editor: 'blue', admin: 'grape' };

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

const CLOSED_KEY = 'fastmock.nav.closed';

function readClosed(): string[] {
  try {
    const v = JSON.parse(readLocal(CLOSED_KEY) ?? '[]') as unknown;
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : [];
  } catch {
    return [];
  }
}

function Nav({ onNavigate, collapsed = false }: { onNavigate?: () => void; collapsed?: boolean }) {
  const { permissions } = useAuth();
  const tokensApi = useTokensApi() === 'available';
  const features = useVersion().data?.features;
  const loc = useLocation();
  const [closed, setClosed] = useState<string[]>(readClosed);
  const groups = visibleNav(permissions, tokensApi, features);
  const toggle = (g: string) =>
    setClosed((c) => {
      const next = c.includes(g) ? c.filter((x) => x !== g) : [...c, g];
      writeLocal(CLOSED_KEY, JSON.stringify(next));
      return next;
    });
  return (
    <nav aria-label="Main" className={classes.nav} data-collapsed={collapsed || undefined}>
      {groups.map((g) => {
        const holdsActive = g.items.some((i) => isActive(i, loc.pathname));
        const open = collapsed || holdsActive || !closed.includes(g.group);
        const single = g.items.length === 1 && g.group === 'Overview';
        return (
          <div key={g.group} className={classes.navGroup}>
            {collapsed ? (
              <div className={classes.groupRule} aria-hidden />
            ) : single ? null : (
              <UnstyledButton className={classes.group} onClick={() => toggle(g.group)} aria-expanded={open} disabled={holdsActive} aria-label={`${g.group} section`}>
                <span>{g.group}</span>
                <IconChevronDown size={14} className={classes.chevron} data-open={open || undefined} aria-hidden />
              </UnstyledButton>
            )}
            {open &&
              g.items.map((i) => {
                const active = isActive(i, loc.pathname);
                const link = (
                  <NavLink
                    key={i.to}
                    component={RouterNavLink}
                    to={i.to}
                    label={collapsed ? undefined : i.label}
                    aria-label={collapsed ? i.label : undefined}
                    leftSection={<i.icon size={19} stroke={1.6} />}
                    active={active}
                    onClick={onNavigate}
                    variant="light"
                    className={classes.navLink}
                  />
                );
                return collapsed ? (
                  <Tooltip key={i.to} label={i.label} position="right" offset={12}>
                    {link}
                  </Tooltip>
                ) : (
                  link
                );
              })}
          </div>
        );
      })}
    </nav>
  );
}

function isMac(): boolean {
  return typeof navigator !== 'undefined' && /Mac|iPhone|iPad/i.test(navigator.userAgent);
}

function SearchButton({ onOpen }: { onOpen: () => void }) {
  const mod = isMac() ? '⌘' : 'Ctrl';
  return (
    <>
      <UnstyledButton className={classes.search} onClick={onOpen} aria-label={`Search pages, stubs and requests (${mod}+K)`} data-testid="open-palette" visibleFrom="sm">
        <IconSearch size={16} aria-hidden />
        <span className={classes.searchText}>Search or jump to…</span>
        <span className="fm-kbd" aria-hidden>
          {mod} K
        </span>
      </UnstyledButton>
      <ActionIcon variant="default" size="lg" onClick={onOpen} aria-label="Search pages, stubs and requests" hiddenFrom="sm">
        <IconSearch size={18} />
      </ActionIcon>
    </>
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

function Principal() {
  const { name, role, kind, info, permissions, logout } = useAuth();
  const summary = accessSummary(permissions, role);
  const badge = summaryLabel(summary);
  const color = ROLE_COLOR[summary.global ?? summary.allSpaces ?? summary.spaces[0]?.role ?? ''] ?? 'teal';
  const groups = info?.principal.groups ?? [];
  const who = name || 'anonymous';
  return (
    <Menu position="bottom-end" withArrow width={280}>
      <Menu.Target>
        <UnstyledButton aria-label={`Account: ${who}`} data-testid="principal" className={classes.principal}>
          <Group gap={8} wrap="nowrap">
            <span className={classes.avatar} aria-hidden>
              {(who.trim()[0] ?? '?').toUpperCase()}
            </span>
            <Text size="sm" fw={500} visibleFrom="sm" className="truncate" maw={160} data-testid="principal-name">
              {who}
            </Text>
            <span className={classes.kind}>
              <Badge color="gray" variant="outline" data-testid="kind-badge" tt="none">
                {kindLabel(kind)}
              </Badge>
            </span>
            <Badge color={color} variant="light" data-testid="role-badge" className={classes.roleBadge} tt="none" title={badge}>
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
          </Text>
          <div data-testid="principal-summary">
            {summary.global && (
              <Text size="xs" c="dimmed">
                Global: {summary.global}
              </Text>
            )}
            {summary.allSpaces && (
              <Text size="xs" c="dimmed">
                All spaces: {summary.allSpaces}
              </Text>
            )}
            {summary.spaces.map((s) => (
              <Text key={s.space} size="xs" c="dimmed" className="truncate">
                {s.space}: {s.role}
              </Text>
            ))}
          </div>
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

const COLLAPSED_KEY = 'fastmock.nav.collapsed';

export function Shell({ children }: { children: ReactNode }) {
  const [opened, { toggle, close }] = useDisclosure(false);
  const [palette, paletteCtl] = useDisclosure(false);
  const [collapsed, setCollapsed] = useState(() => readLocal(COLLAPSED_KEY) === '1');
  const { info } = useAuth();
  const spaces = useSpace();
  useHotkeys([['mod+K', () => paletteCtl.toggle()]], []);
  const flipCollapsed = () => {
    setCollapsed((c) => {
      writeLocal(COLLAPSED_KEY, c ? '0' : '1');
      return !c;
    });
  };
  return (
    <div className={classes.root} data-collapsed={collapsed || undefined}>
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
            <span className={`fm-chip ${classes.version}`}>v{info.version}</span>
          )}
          <div className={classes.searchSlot}>
            <SearchButton onOpen={paletteCtl.open} />
          </div>
        </Group>
        <Group gap="xs" wrap="nowrap" className={classes.headerRight}>
          <div className={classes.spaceSlot}>
            <SpaceSelect />
          </div>
          <ThemeToggle />
          <Principal />
        </Group>
      </header>
      <div className={classes.body}>
        <div className={classes.sideCol}>
          <aside className={classes.sidebar}>
            <ScrollArea className={classes.sideScroll} scrollbarSize={6} type="hover">
              <Nav collapsed={collapsed} />
            </ScrollArea>
            <div className={classes.sideFoot}>
              <Tooltip label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'} position="right" disabled={!collapsed}>
                <UnstyledButton className={classes.collapse} onClick={flipCollapsed} aria-label={collapsed ? 'Expand sidebar' : 'Collapse sidebar'} aria-pressed={collapsed}>
                  {collapsed ? <IconLayoutSidebarLeftExpand size={18} /> : <IconLayoutSidebarLeftCollapse size={18} />}
                  {!collapsed && <span>Collapse</span>}
                </UnstyledButton>
              </Tooltip>
            </div>
          </aside>
        </div>
        <Drawer opened={opened} onClose={close} size={288} title={
            <span className={classes.drawerTitle}>
              <img src={lockupLight} alt="FastMock" width={163} height={22} className="fm-on-light" />
              <img src={lockupDark} alt="FastMock" width={163} height={22} className="fm-on-dark" />
            </span>
          } hiddenFrom="md" classNames={{ body: classes.drawerBody }}>
          <Nav onNavigate={close} />
        </Drawer>
        <main id="main" className={classes.main} tabIndex={-1}>
          <div className={classes.mainInner}>
            <PageGuard>{children}</PageGuard>
          </div>
        </main>
      </div>
      <CommandPalette opened={palette} onClose={paletteCtl.close} />
    </div>
  );
}
