import { Anchor, Button, Group, Text, Title } from '@mantine/core';
import { IconArrowRight, IconBook2, IconGauge } from '@tabler/icons-react';
import { Link, useLocation } from 'react-router';
import { useTokensApi } from '../api/features';
import { useAuth } from '../auth/useAuth';
import { visibleNav } from '../components/nav';
import { docsUrl } from '../lib/docs';
import { suggestScreens } from '../lib/screens';
import classes from './NotFound.module.css';

export default function NotFound() {
  const { pathname } = useLocation();
  const { permissions } = useAuth();
  const tokensApi = useTokensApi() === 'available';
  const items = visibleNav(permissions, tokensApi)
    .flatMap((g) => g.items)
    .filter((i) => i.to !== '/');
  const suggestions = suggestScreens(pathname, items, ['/stubs', '/requests', '/near-misses', '/services']);
  return (
    <section className={classes.wrap} data-testid="not-found">
      <div className={classes.card}>
        <div className={classes.code} aria-hidden>
          404
        </div>
        <Title order={2} className={classes.title}>
          Page not found
        </Title>
        <Text c="dimmed" ta="center" maw={520}>
          Nothing lives at <span className="mono">{pathname}</span>. The link may be old, or the screen may need a permission you do not have.
        </Text>
        <Group gap="sm" justify="center" mt="xs">
          <Button component={Link} to="/" leftSection={<IconGauge size={16} />}>
            Back to the dashboard
          </Button>
          <Button component="a" href={docsUrl('guide/ui')} target="_blank" rel="noopener noreferrer" variant="default" leftSection={<IconBook2 size={16} />}>
            UI guide
          </Button>
        </Group>
        {suggestions.length > 0 && (
          <nav aria-label="Suggested screens" className={classes.suggest}>
            <span className="fm-subhead">Perhaps you meant</span>
            <ul className={classes.list}>
              {suggestions.map((s) => {
                const Icon = s.icon;
                return (
                  <li key={s.to}>
                    <Anchor component={Link} to={s.to} className={classes.link} underline="never">
                      <span className="fm-section-icon" aria-hidden>
                        <Icon size={16} />
                      </span>
                      <span className={classes.linkLabel}>{s.label}</span>
                      <IconArrowRight size={14} aria-hidden className={classes.arrow} />
                    </Anchor>
                  </li>
                );
              })}
            </ul>
          </nav>
        )}
        <Text size="xs" c="dimmed">
          Tip: press <span className="fm-kbd">Ctrl</span> <span className="fm-kbd">K</span> to search for a screen or a stub.
        </Text>
      </div>
    </section>
  );
}
