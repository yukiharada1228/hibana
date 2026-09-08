import React from 'react';
import {Composition, registerRoot} from 'remotion';
import {HibanaDemo} from './video';

const Root = () => <Composition id="HibanaDemo" component={HibanaDemo} width={1920} height={1080} fps={30} durationInFrames={2700}/>;
registerRoot(Root);
