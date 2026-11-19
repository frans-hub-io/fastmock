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
  IconWebhook,
} from '@tabler/icons-react';
import type { ComponentType, ReactNode } from 'react';
import { Link, NavLink as RouterNavLink, useLocation } from 'react-router';
import type { Role } from '../api/types';
import { hasRole, useAuth } from '../auth/useAuth';
import classes from './Shell.module.css';

interface NavItem {
  to: string;
  label: string;
  icon: ComponentType<{ size?: number; stroke?: number }>;
  role?: Role;
}

const NAV: { group: string; items: NavItem[] }[] = [
  { group: 'Overview', items: [{ to: '/', label: 'Dashboard', icon: IconGauge }] },
  {
    group: 'Mocks',
    items: [
      { to: '/stubs', label: 'Stubs', icon: IconRoute },
      { to: '/services', label: 'Services', icon: IconSitemap },
      { to: '/scenarios', label: 'Scenarios', icon: IconChecklist },
      { to: '/async', label: 'Async mocks', icon: IconBroadcast },
      { to: '/grpc', label: 'gRPC', icon: IconFileCode },
      { to: '/files', label: 'Files', icon: IconFiles },
    ],
  },
  {
    group: 'Traffic',
    items: [
      { to: '/requests', label: 'Request journal', icon: IconActivity },
      { to: '/near-misses', label: 'Near misses', icon: IconSearch },
      { to: '/verify', label: 'Verify', icon: IconShieldCheck },
      { to: '/callbacks', label: 'Callbacks', icon: IconWebhook },
      { to: '/recordings', label: 'Recordings', icon: IconPlayerRecord },
    ],
  },
  {
    group: 'Testing',
    items: [
      { to: '/contract-tests', label: 'Contract tests', icon: IconBox },
      { to: '/chaos', label: 'Chaos', icon: IconFlame },
    ],
  },
  {
    group: 'System',
    items: [
      { to: '/kv', label: 'KV store', icon: IconDatabase },
      { to: '/settings', label: 'Settings', icon: IconSettings },
      { to: '/audit', label: 'Audit log', icon: IconHistory, role: 'admin' },
      { to: '/api', label: 'API docs', icon: IconApi },
    ],
  },
];

const ROLE_COLOR: Record<Role, string> = { viewer: 'gray', editor: 'blue', admin: 'grape' };

function Nav({ onNavigate }: { onNavigate?: () => void }) {
  const { role } = useAuth();
  const loc = useLocation();
  return (
    <nav aria-label="Main">
      {NAV.map((g) => (
        <div key={g.group}>
          <div className={classes.group}>{g.group}</div>
          {g.items
            .filter((i) => !i.role || hasRole(role, i.role))
            .map((i) => {
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

export function Shell({ children }: { children: ReactNode }) {
  const [opened, { toggle, close }] = useDisclosure(false);
  const { name, role, info, logout } = useAuth();
  return (
    <div className={classes.root}>
      <a href="#main" className={classes.skip}>
        Skip to content
      </a>
      <header className={classes.header}>
        <Group gap="sm" wrap="nowrap">
          <Burger opened={opened} onClick={toggle} hiddenFrom="md" size="sm" aria-label="Toggle navigation" />
          <Link to="/" className={classes.brand}>
            <span className={classes.logo} aria-hidden>
              F
            </span>
            <span>FastMock</span>
          </Link>
          {info?.version && (
            <Badge variant="outline" color="gray" size="sm" visibleFrom="sm">
              v{info.version}
            </Badge>
          )}
        </Group>
        <Group gap="xs" wrap="nowrap">
          <ThemeToggle />
          <Menu position="bottom-end" withArrow>
            <Menu.Target>
              <UnstyledButton aria-label="Account" data-testid="principal">
                <Group gap={8} wrap="nowrap">
                  <IconUser size={18} />
                  <Text size="sm" fw={500} visibleFrom="sm" className="truncate" maw={160}>
                    {name || 'anonymous'}
                  </Text>
                  <Badge color={ROLE_COLOR[role]} variant="light" data-testid="role-badge">
                    {role}
                  </Badge>
                </Group>
              </UnstyledButton>
            </Menu.Target>
            <Menu.Dropdown>
              <Menu.Label>
                Signed in as {name || 'anonymous'} ({role})
              </Menu.Label>
              {info?.authDisabled && <Menu.Label c="orange">Authentication is disabled on this server</Menu.Label>}
              <Menu.Item leftSection={<IconBolt size={16} />} component={Link} to="/">
                Dashboard
              </Menu.Item>
              {!info?.authDisabled && (
                <Menu.Item leftSection={<IconLogout size={16} />} color="red" onClick={() => logout()} data-testid="logout">
                  Sign out
                </Menu.Item>
              )}
            </Menu.Dropdown>
          </Menu>
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
          {children}
        </main>
      </div>
    </div>
  );
}
