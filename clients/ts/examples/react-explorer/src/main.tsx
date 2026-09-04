import {StrictMode} from 'react';
import {createRoot} from 'react-dom/client';
import {App} from './App.js';

// StrictMode on purpose: its double mount is the case `useTesseraStore` is built for.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>
);
