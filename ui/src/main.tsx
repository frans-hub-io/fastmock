import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import '@mantine/core/styles.css';
import '@mantine/notifications/styles.css';
import '@mantine/dropzone/styles.css';
import './assets/fonts/fonts.css';
import './styles/brand-tokens.css';
import './styles/mantine-theme.css';
import './styles/global.css';
import { App } from './App';
import { setNonce } from 'get-nonce';
import { applyInitialColorScheme, syncThemeAttribute } from './lib/colorScheme';
import { styleNonce } from './lib/nonce';

applyInitialColorScheme();
syncThemeAttribute();
const nonce = styleNonce();
if (nonce) setNonce(nonce);

const root = document.getElementById('root');
if (root) {
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
