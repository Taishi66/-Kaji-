import { useState, useEffect } from 'react';
import { Button } from '../../ui/button';
import { Check } from '../../icons';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../../ui/dialog';
import { errorMessage } from '../../../utils/conversionUtils';
import { defineMessages, useIntl } from '../../../i18n';

const i18n = defineMessages({
  dialogTitle: {
    id: 'kajihintsModal.dialogTitle',
    defaultMessage: 'Configure Project Hints (.kajihints)',
  },
  dialogDescription: {
    id: 'kajihintsModal.dialogDescription',
    defaultMessage:
      'Provide additional context about your project to improve communication with Kaji',
  },
  helpText1: {
    id: 'kajihintsModal.helpText1',
    defaultMessage:
      '.kajihints is a text file used to provide additional context about your project and improve the communication with Kaji.',
  },
  helpText2: {
    id: 'kajihintsModal.helpText2',
    defaultMessage:
      "Please make sure {bold} extension is enabled in the extensions page. This extension is required to use .kajihints. You'll need to restart your session for .kajihints updates to take effect.",
  },
  helpText3: {
    id: 'kajihintsModal.helpText3',
    defaultMessage: 'See {link} for more information.',
  },
  helpTextLink: {
    id: 'kajihintsModal.helpTextLink',
    defaultMessage: 'using .kajihints',
  },
  errorReading: {
    id: 'kajihintsModal.errorReading',
    defaultMessage: 'Error reading .kajihints file: {error}',
  },
  fileFound: {
    id: 'kajihintsModal.fileFound',
    defaultMessage: '.kajihints file found at: {filePath}',
  },
  fileCreating: {
    id: 'kajihintsModal.fileCreating',
    defaultMessage: 'Creating new .kajihints file at: {filePath}',
  },
  placeholder: {
    id: 'kajihintsModal.placeholder',
    defaultMessage: 'Enter project hints here...',
  },
  savedSuccessfully: {
    id: 'kajihintsModal.savedSuccessfully',
    defaultMessage: 'Saved successfully',
  },
  close: {
    id: 'kajihintsModal.close',
    defaultMessage: 'Close',
  },
  saving: {
    id: 'kajihintsModal.saving',
    defaultMessage: 'Saving...',
  },
  save: {
    id: 'kajihintsModal.save',
    defaultMessage: 'Save',
  },
  failedToAccess: {
    id: 'kajihintsModal.failedToAccess',
    defaultMessage: 'Failed to access .kajihints file',
  },
  failedToSave: {
    id: 'kajihintsModal.failedToSave',
    defaultMessage: 'Failed to save .kajihints file',
  },
  developer: {
    id: 'kajihintsModal.developer',
    defaultMessage: 'Developer',
  },
});

const HelpText = () => {
  const intl = useIntl();

  return (
    <div className="text-sm flex-col space-y-4 text-text-secondary">
      <p>{intl.formatMessage(i18n.helpText1)}</p>
      <p>
        {intl.formatMessage(i18n.helpText2, {
          bold: <span className="font-bold">{intl.formatMessage(i18n.developer)}</span>,
        })}
      </p>
      <p>
        {intl.formatMessage(i18n.helpText3, {
          link: (
            <Button
              variant="link"
              className="text-blue-500 hover:text-blue-600 p-0 h-auto"
              onClick={() =>
                window.open(
                  'https://goose-docs.ai/docs/guides/using-goosehints/',
                  '_blank'
                )
              }
            >
              {intl.formatMessage(i18n.helpTextLink)}
            </Button>
          ),
        })}
      </p>
    </div>
  );
};

const ErrorDisplay = ({ error }: { error: Error }) => {
  const intl = useIntl();

  return (
    <div className="text-sm text-text-secondary">
      <div className="text-red-600">
        {intl.formatMessage(i18n.errorReading, { error: errorMessage(error) })}
      </div>
    </div>
  );
};

const FileInfo = ({ filePath, found }: { filePath: string; found: boolean }) => {
  const intl = useIntl();

  return (
    <div className="text-sm font-medium mb-2">
      {found ? (
        <div className="text-green-600">
          <Check className="w-4 h-4 inline-block" />{' '}
          {intl.formatMessage(i18n.fileFound, { filePath })}
        </div>
      ) : (
        <div>{intl.formatMessage(i18n.fileCreating, { filePath })}</div>
      )}
    </div>
  );
};

const getKajihintsFile = async (filePath: string) => await window.electron.readFile(filePath);

interface KajihintsModalProps {
  directory: string;
  setIsKajihintsModalOpen: (isOpen: boolean) => void;
}

export const KajihintsModal = ({ directory, setIsKajihintsModalOpen }: KajihintsModalProps) => {
  const intl = useIntl();
  const kajihintsFilePath = `${directory}/.kajihints`;
  const [kajihintsFile, setKajihintsFile] = useState<string>('');
  const [kajihintsFileFound, setKajihintsFileFound] = useState<boolean>(false);
  const [kajihintsFileReadError, setKajihintsFileReadError] = useState<string>('');
  const [isSaving, setIsSaving] = useState(false);
  const [saveSuccess, setSaveSuccess] = useState(false);

  useEffect(() => {
    const fetchKajihintsFile = async () => {
      try {
        const { file, error, found } = await getKajihintsFile(kajihintsFilePath);
        setKajihintsFile(file);
        setKajihintsFileFound(found);
        setKajihintsFileReadError(found && error ? error : '');
      } catch (error) {
        console.error('Error fetching .kajihints file:', error);
        setKajihintsFileReadError(intl.formatMessage(i18n.failedToAccess));
      }
    };
    if (directory) fetchKajihintsFile();
  }, [directory, kajihintsFilePath, intl]);

  const writeFile = async () => {
    setIsSaving(true);
    setSaveSuccess(false);
    try {
      await window.electron.writeFile(kajihintsFilePath, kajihintsFile);
      setSaveSuccess(true);
      setKajihintsFileFound(true);
      setTimeout(() => setSaveSuccess(false), 3000);
    } catch (error) {
      console.error('Error writing .kajihints file:', error);
      setKajihintsFileReadError(intl.formatMessage(i18n.failedToSave));
    } finally {
      setIsSaving(false);
    }
  };

  return (
    <Dialog open={true} onOpenChange={(open) => setIsKajihintsModalOpen(open)}>
      <DialogContent className="w-[80vw] max-w-[80vw] sm:max-w-[80vw] max-h-[90vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>{intl.formatMessage(i18n.dialogTitle)}</DialogTitle>
          <DialogDescription>{intl.formatMessage(i18n.dialogDescription)}</DialogDescription>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 pt-2 pb-4">
          <HelpText />

          <div>
            {kajihintsFileReadError ? (
              <ErrorDisplay error={new Error(kajihintsFileReadError)} />
            ) : (
              <div className="space-y-2">
                <FileInfo filePath={kajihintsFilePath} found={kajihintsFileFound} />
                <textarea
                  value={kajihintsFile}
                  className="w-full h-80 border rounded-md p-2 text-sm resize-none bg-background-primary text-text-primary border-border-primary focus:outline-none focus:ring-2 focus:ring-blue-500"
                  onChange={(event) => setKajihintsFile(event.target.value)}
                  placeholder={intl.formatMessage(i18n.placeholder)}
                />
              </div>
            )}
          </div>
        </div>

        <DialogFooter>
          {saveSuccess && (
            <span className="text-green-600 text-sm flex items-center gap-1 mr-auto">
              <Check className="w-4 h-4" />
              {intl.formatMessage(i18n.savedSuccessfully)}
            </span>
          )}
          <Button variant="outline" onClick={() => setIsKajihintsModalOpen(false)}>
            {intl.formatMessage(i18n.close)}
          </Button>
          <Button onClick={writeFile} disabled={isSaving}>
            {isSaving ? intl.formatMessage(i18n.saving) : intl.formatMessage(i18n.save)}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
};
