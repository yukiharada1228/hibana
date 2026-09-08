import {Config} from '@remotion/cli/config';
import {existsSync} from 'node:fs';
const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
if (existsSync(chrome)) Config.setBrowserExecutable(chrome);
Config.setOverwriteOutput(true);
