/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.csswg.org/web-animations-1/#the-animationplaybackevent-interface

[Exposed=Window, Pref="dom_web_animations_enabled"]
interface AnimationPlaybackEvent : Event {
  constructor(DOMString type, optional AnimationPlaybackEventInit eventInitDict = {});
  readonly attribute double? currentTime;
  readonly attribute double? timelineTime;
};

dictionary AnimationPlaybackEventInit : EventInit {
  double? currentTime = null;
  double? timelineTime = null;
};
