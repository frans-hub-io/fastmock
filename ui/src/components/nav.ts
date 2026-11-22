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
  IconInbox,
  IconKey,
  IconMail,
  IconPlayerRecord,
  IconRoute,
  IconSearch,
  IconSettings,
  IconShieldCheck,
  IconSitemap,
  IconUserShield,
  IconWebhook,
} from '@tabler/icons-react';
import type { ComponentType } from 'react';
import { can, type Permission, type Permissions } from '../auth/permissions';

export interface NavItem {
  to: string;
  label: string;
  icon: ComponentType<{ size?: number; stroke?: number }>;
  perm?: Permission;
  /** The permission must be held globally (the screen is not scoped by space). */
  global?: boolean;
  /** Shown only when the server offers the managed-token API. */
  tokensApi?: boolean;
  /** Shown only when `GET /__admin/version` lists this feature. */
  feature?: string;
  /** Extra words the command palette matches. */
  keywords?: string;
}

export interface NavGroup {
  group: string;
  items: NavItem[];
}

export const NAV: NavGroup[] = [
  { group: 'Overview', items: [{ to: '/', label: 'Dashboard', icon: IconGauge, keywords: 'home overview metrics traffic' }] },
  {
    group: 'Mocks',
    items: [
      { to: '/quick-mock', label: 'Quick mock', icon: IconBolt, perm: 'stubs:write', keywords: 'new mock wizard first start template create' },
      { to: '/stubs', label: 'Stubs', icon: IconRoute, perm: 'stubs:read', keywords: 'mappings routes mocks' },
      { to: '/services', label: 'Services', icon: IconSitemap, perm: 'stubs:read', global: true, keywords: 'catalog openapi specs' },
      { to: '/scenarios', label: 'Scenarios', icon: IconChecklist, perm: 'state:read', keywords: 'state machine' },
      { to: '/async', label: 'Async mocks', icon: IconBroadcast, perm: 'stubs:read', global: true, keywords: 'kafka asyncapi events' },
      { to: '/grpc', label: 'gRPC', icon: IconFileCode, perm: 'grpc:read', keywords: 'protobuf descriptors' },
      { to: '/files', label: 'Files', icon: IconFiles, perm: 'files:read', keywords: 'body files' },
    ],
  },
  {
    group: 'Traffic',
    items: [
      { to: '/requests', label: 'Request journal', icon: IconActivity, perm: 'journal:read', keywords: 'requests log history' },
      { to: '/near-misses', label: 'Near misses', icon: IconSearch, perm: 'journal:read', keywords: 'unmatched explain diff' },
      { to: '/verify', label: 'Verify', icon: IconShieldCheck, perm: 'journal:read', keywords: 'count assert sequence' },
      { to: '/mail', label: 'Mail', icon: IconMail, perm: 'mail:read', feature: 'mail', keywords: 'email smtp inbox messages imap pop3' },
      { to: '/messages', label: 'Messages', icon: IconInbox, perm: 'messages:read', feature: 'capture', keywords: 'broker capture kafka mqtt amqp rabbitmq topics events' },
      { to: '/callbacks', label: 'Callbacks', icon: IconWebhook, perm: 'callbacks:read', keywords: 'webhooks outbox' },
      { to: '/recordings', label: 'Recordings', icon: IconPlayerRecord, perm: 'recordings:read', keywords: 'record proxy snapshot' },
    ],
  },
  {
    group: 'Testing',
    items: [
      { to: '/contract-tests', label: 'Contract tests', icon: IconBox, perm: 'contracts:read', keywords: 'openapi conformance' },
      { to: '/chaos', label: 'Chaos', icon: IconFlame, perm: 'chaos:read', keywords: 'faults latency' },
    ],
  },
  {
    group: 'System',
    items: [
      { to: '/kv', label: 'KV store', icon: IconDatabase, perm: 'state:read', keywords: 'key value state' },
      { to: '/settings', label: 'Settings', icon: IconSettings, perm: 'chaos:read', keywords: 'configuration' },
      { to: '/tokens', label: 'Admin tokens', icon: IconKey, perm: 'tokens:read', tokensApi: true, keywords: 'api keys credentials' },
      { to: '/access', label: 'Access', icon: IconUserShield, keywords: 'roles permissions spaces bindings' },
      { to: '/audit', label: 'Audit log', icon: IconHistory, perm: 'audit:read', keywords: 'changes history who' },
      { to: '/api', label: 'API docs', icon: IconApi, keywords: 'openapi swagger reference' },
    ],
  },
];

/** The navigation entry a path belongs to: the deepest entry that is the path or one of its parents. */
export function navItemOf(pathname: string): NavItem | undefined {
  return NAV.flatMap((g) => g.items)
    .filter((i) => i.to !== '/' && (pathname === i.to || pathname.startsWith(`${i.to}/`)))
    .sort((a, b) => b.to.length - a.to.length)[0];
}

export function isActive(item: NavItem, pathname: string): boolean {
  return item.to === '/' ? pathname === '/' : pathname === item.to || pathname.startsWith(`${item.to}/`);
}

/** The groups and entries the principal may open; entries of optional features only when `features` lists them. */
export function visibleNav(permissions: Permissions | null | undefined, tokensApi: boolean, features: readonly string[] = []): NavGroup[] {
  const shown = (i: NavItem) =>
    (!i.perm || can(permissions, i.perm, i.global ? null : undefined)) && (!i.tokensApi || tokensApi) && (!i.feature || features.includes(i.feature));
  return NAV.map((g) => ({ ...g, items: g.items.filter(shown) })).filter((g) => g.items.length > 0);
}

/** The navigation group a path belongs to. */
export function navGroupOf(pathname: string): string | undefined {
  if (pathname === '/') return NAV[0]?.group;
  const item = navItemOf(pathname);
  return item ? NAV.find((g) => g.items.includes(item))?.group : undefined;
}
