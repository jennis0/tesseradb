import {StrictMode} from 'react';
import {createRoot} from 'react-dom/client';
import {App} from './App.js';

// StrictMode, whose double mount `useMosaicaStore` handles.
createRoot(document.getElementById('root')!).render(
  <StrictMode>
    <App />
  </StrictMode>
);
