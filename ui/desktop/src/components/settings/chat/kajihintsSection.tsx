import { useState } from 'react';
import { Button } from '../../ui/button';
import { FolderKey } from 'lucide-react';
import { KajihintsModal } from './KajihintsModal';
import { defineMessages, useIntl } from '../../../i18n';

const i18n = defineMessages({
  title: {
    id: 'kajihintsSection.title',
    defaultMessage: 'Project Hints (.kajihints)',
  },
  description: {
    id: 'kajihintsSection.description',
    defaultMessage:
      "Configure your project's .kajihints file to provide additional context to Kaji",
  },
  configure: {
    id: 'kajihintsSection.configure',
    defaultMessage: 'Configure',
  },
});

export const KajihintsSection = () => {
  const intl = useIntl();
  const [isModalOpen, setIsModalOpen] = useState(false);
  const directory = window.appConfig?.get('KAJI_WORKING_DIR') as string;

  return (
    <>
      <div className="flex items-center justify-between px-2 py-2">
        <div className="flex-1">
          <h3 className="text-text-primary">{intl.formatMessage(i18n.title)}</h3>
          <p className="text-xs text-text-secondary mt-[2px]">
            {intl.formatMessage(i18n.description)}
          </p>
        </div>
        <Button
          onClick={() => setIsModalOpen(true)}
          variant="outline"
          size="sm"
          className="flex items-center gap-2"
        >
          <FolderKey size={16} />
          {intl.formatMessage(i18n.configure)}
        </Button>
      </div>
      {isModalOpen && (
        <KajihintsModal directory={directory} setIsKajihintsModalOpen={setIsModalOpen} />
      )}
    </>
  );
};
