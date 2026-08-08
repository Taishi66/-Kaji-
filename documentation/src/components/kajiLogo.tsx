import { useColorMode } from '@docusaurus/theme-common';

export const KajiLogo = (props: { className?: string }) => {
  const { colorMode } = useColorMode();
  
  const logoSrc = colorMode === 'dark' 
    ? 'img/kaji-logo-white.png' 
    : 'img/kaji-logo-black.png';
  
  const logoAlt = 'kaji logo';

  return (
    <img
      src={logoSrc}
      alt={logoAlt}
      className={props.className}
      style={{ height: 'auto', maxWidth: '100%' }}
    />
  );
};
