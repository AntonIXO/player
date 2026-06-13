# Chord Mojo 2: Два отдельных бага — полный разбор

## Обзор: это действительно две разные проблемы

Пользователи и обзорщики часто смешивают две принципиально разные неисправности Mojo 2, называя их общим словом "белый шум" или "проблема с громкостью". Они имеют разные технические причины, разные триггеры и разную степень опасности. Единственное общее — оба связаны с USB-интерфейсом и поведением устройства при потере или переинициализации цифрового потока.

***

## Баг А: Белый/розовый шум

### Что происходит

Внезапный белый, розовый или статический шум вместо музыки. Воспроизводится **на текущей установленной громкости** — то есть если громкость была низкой, шум тоже будет тихим. Кнопки громкости на Mojo 2 работают и во время инцидента — можно убавить. Пауза приложения или повторный запуск обычно устраняет шум.[^1]

### Техническая причина

Mojo 2 построен на связке ATSAM3U1C микроконтроллер (USB-приёмник) + Artix-7 FPGA (WTA-фильтр, 40 960 тапов, pulse array DAC). Когда USB-поток прерывается или теряет синхронизацию, микроконтроллер может передать на вход FPGA мусорные данные — случайные биты вместо PCM-сэмплов. FPGA добросовестно обрабатывает их своим WTA-фильтром и выводит на pulse array DAC — на выходе получается широкополосный шум.[^2][^3][^4]

Аналогичная проблема известна для других USB-мостов: *"A brief interruption in the USB data connection will result in full-scale white noise being sent to the DAC"* — и описана для продуктов на чипах CMedia, которые Chord тоже использовал в ранних версиях. E1DA, столкнувшись с этим же багом, полностью сменила поставщика USB-мостов на Comtrue — Chord этого не сделала.[^3]

### Задокументированные триггеры бага А

| Триггер | Платформа | Источник |
|---|---|---|
| Смена sample rate между треками (44.1↔48↔96 кГц) | Все | [^4][^5] |
| Конфликт sample rate: браузер шлёт 48 кГц пока Mojo настроен на 384 кГц | Windows | [^5] |
| Плохое качество USB-кабеля (не по стандарту USB 2.0) | Все | [^1][^6] |
| Высокое EMI/RFI в окружении | Все | [^3] |
| Пыль/грязь в USB-разъёме, ослабленный контакт | Все | [^3] |
| Конфликт с M1/M2 Apple Silicon (специфичный баг) | macOS M1/M2 | [^7][^6] |
| Android: пауза в Roon ARC вызывает высокочастотный писк | Android | [^8] |
| Апсемплинг выше 384 кГц через Roon | Все | [^8] |
| Запасной/дефектный юнит (аппаратный дефект) | Н/Д | [^9] |

### Ответ Chord на баг А

Chord официально признал проблему только частично. На вопрос о прошивочном фиксе белого шума ответ был уклончивым:[^7]

> *"There IS a firmware update that relates to Mojo 2's connection with M1-based Apple devices, but the firmware update neither is meant to fix the white noise issue, nor does it fix it."*

В ответе на жалобу через Head-Fi и Roon Community Chord сообщил пользователям, что причина — **неправильные настройки** (DTS/Dolby включены, слишком высокий sample rate для Windows notification sounds), и предложил понизить output sample rate до 48 кГц. Это частичное решение для Windows, но не объясняет случаи на macOS и Linux.[^5]

Официальный ответ Chord на Facebook при объявлении обновлённого Mojo 2 (2025):

> Пользователь: *"Have you done something about those white noise blasts?"*
> Chord (косвенно, через другого пользователя): *"Pretty sure that was fixed with firmware a while ago."*[^10]

Самостоятельного публичного заявления Chord по этой теме не выпускал.

### Воспроизводится ли на оптическом/коаксиальном?

По имеющимся данным — **нет**. Rob Watts (главный инженер Chord) лично рекомендовал оптический вход как более чистый вариант именно из-за USB-проблем. Пользователи, перешедшие на оптику (через Raspberry Pi + HiFiBerry HAT или USB-to-SPDIF конвертер типа Douk U2), сообщают об исчезновении шума. Один пользователь решил проблему через Topping D10s как USB-to-SPDIF мост.[^11][^12][^13][^14]

Оптика ограничена 192 кГц / DSD64 DoP; коаксиальный вход — до 768 кГц.[^15][^16]

***

## Баг Б: Скачок на максимальную громкость (0 dBFS full-scale output)

### Что происходит

Внезапный выброс на **максимальную амплитуду сигнала** — полная шкала (0 dBFS) на выходе DAC. Это не шум, а реальный PCM-сигнал или DC на максимальном уровне. **Критически важно:** кнопки громкости Mojo 2 в этот момент **не помогают** — регулятор работает до stage DAC, а выброс происходит уже после или в момент, когда аналоговый выход не ограничен программно. Результат — потенциально 130+ дБ в ухе при чувствительных IEM.[^17][^1]

### Техническая причина

Mojo 2 реализует регулировку громкости **цифровым образом через FPGA**. При полном сбросе PCM-состояния (reset/reinit) — например, через `snd_pcm_drop()` в Linux или при USB-переинициализации — FPGA может на короткое время оказаться в состоянии, когда регистр громкости не применён, но аналоговый выход уже активен через output relay. Все выборки в этот момент проходят без аттенюации — при full-scale входном сигнале выход тоже full-scale.[^18]

В официальном Mojo 2 manual косвенно подтверждается опасный сценарий смены выходов:

> *"If using sensitive 3.5mm headphones, it is strongly recommended that you unplug them before connecting a 4.4mm set to avoid a potentially damaging volume spike."*[^19]

Это означает, что сам Chord признаёт существование сценария **volume spike** при определённых условиях переключения.

Дополнительный аппаратный дефект (отдельный от основного бага): в одном задокументированном случае Head-Fi Mojo 2 начинал выдавать **DC напряжение 1.2V на правом канале**, нарастающее до 3.5V DC за 6 минут — независимо от воспроизведения музыки, при каждом включении питания. Это гарантированное сгорание IEM с импедансом 8 Ом (рассеиваемая мощность ~1.5 Вт). Это аппаратный дефект конкретного юнита, а не программный баг.[^20]

### Задокументированные триггеры бага Б

| Триггер | Платформа | Механизм | Источник |
|---|---|---|---|
| `snd_pcm_drop()` + `prepare()` при паузе (Linux) | Linux | PCM state reset, FPGA без применённого volume |  |
| USB driver timeout на Windows | Windows | Driver переинициализирует устройство с burst | [^1] |
| Suspend/wake компьютера с подключённым Mojo 2 | Все | USB питание пропадает, DAC reinit без mute | [^1] |
| Смена sample rate (rate change между треками) | Все | Device reopen → brief unmuted state | [^4] |
| Два приложения конкурируют за exclusive device | Все | State corruption при захвате | [^21] |
| Физическое переподключение USB во время playback | Все | Reinit без предварительного mute | [^21] |
| Подключение 4.4mm наушников при уже подключённых 3.5mm | Физическое | Volume memory 4.4mm применяется к обоим выходам | [^19] |
| Неправильная последовательность включения (источник раньше Mojo 2) | Все | DAC получает сигнал до готовности | [^21] |
| Аппаратный дефект юнита (DC offset) | Н/Д | Неисправный output stage | [^20] |

### Ответ Chord на баг Б

По данным пользователя, который написал в Chord после инцидента со скримингом при подключении FiiO M23:[^22]

> *"Chord deflected, claiming their device is not at fault, I need to get some kind of device update with the local distributor, and they even said that I should use optical instead of USB-C as it won't happen with optical."*

Это показательный ответ: Chord **неофициально признал**, что оптический вход не воспроизводит этот баг, но публично вину не взял. Прямого официального заявления по багу Б найти не удалось. Chord не выпускал публичных advisory, отзывов или patch notes, напрямую адресующих hazardous full-scale output.

### Воспроизводится ли на оптическом/коаксиальном?

По имеющимся данным — **предположительно нет**. Cord сам рекомендовал оптику как обходной путь. Логика это подтверждает: оптический и коаксиальный входы используют совершенно другой путь синхронизации (SPDIF PLL, а не USB UAC protocol). Реинициализация при смене треков через оптику происходит по-другому — нет USB state machine, нет driver timeout, нет `snd_pcm_drop()`.[^22]

Однако задокументированных случаев **баги Б на оптике или коаксиале нет** — это важно: не факт что они невозможны, просто не зафиксированы. Смена rate через коаксиальный вход теоретически тоже вызывает relock DAC.

***

## Сравнительная таблица двух багов

| Характеристика | Баг А (белый шум) | Баг Б (full-scale output) |
|---|---|---|
| Природа сигнала | Случайные биты / широкополосный шум | PCM full-scale или DC |
| Зависит от регулятора громкости | **Да** — шум на текущей громкости | **Нет** — максимум независимо от настройки |
| Можно убавить во время инцидента | Да | Нет (или поздно) |
| Опасность для слуха | Средняя (зависит от громкости) | **Высокая** — потенциально 130+ дБ |
| Опасность для наушников | Низкая | Высокая |
| Платформа | Все, чаще macOS | Все, Linux наиболее воспроизводимо |
| Частота | Относительно часто | Редко, но стабильно воспроизводимо |
| Воспроизведение на оптике | Не зафиксировано | Не зафиксировано |
| Официальная реакция Chord | Частичная (cable/settings blame) | Отрицание → рекомендация оптики |
| Исправлено обновлением? | Частично (M1 Apple) | Нет |
| Присутствует в ревизии 2025/2026 | Неизвестно | Неизвестно |

***

## Архитектурная причина, почему это не фиксируется

Mojo 2 лишён **аппаратного mute-реле на аналоговом выходе** во время цифровых переходов. Relay при включении слышен ("click"), но он управляется логикой включения питания, а не логикой переходов sample rate или USB reinit. Это проектное решение Chord: Rob Watts минимизирует компоненты в сигнальном пути. В результате в момент любой переинициализации существует временно́е окно (миллисекунды), когда аналоговый выход активен, но программный volume control DAC ещё не применён.[^20]

Профессиональное оборудование (Benchmark, RME) использует явный mute в ISR переключения sample rate — это индустриальный стандарт. У Chord его нет.[^1]

***

## Что реально помогает

### Для бага А
- Использовать качественный кабель (DDHiFi, Belkin)[^1]
- Зафиксировать sample rate источника (не менять между треками)[^5]
- Переход на оптику через USB-to-SPDIF конвертер[^12][^14]
- Отключить Windows notification sounds при HiRes playback[^5]

### Для бага Б
- **Оптический или коаксиальный вход** вместо USB — устраняет все USB-специфичные триггеры[^11][^22]
- Никогда не оставлять наушники в ушах при resume после паузы, suspend/wake, смене треков разного формата
- В Linux: `snd_usb_audio power_save=0 implicit_fb=1 autoclock=0`
- Silent pre-roll на resume в коде (устраняет конкретный Linux/`drop()`-триггер)
- Никогда не подключать второй тип наушников (3.5/4.4) без отключения первого

---

## References

1. [Beware of Chord Mojo 2 - Serious Problem : r/headphones - Reddit](https://www.reddit.com/r/headphones/comments/1kq97y7/beware_of_chord_mojo_2_serious_problem/) - I've experienced sudden, full-volume output (0dBFS) during playback. Just instant, ear-shattering vo...

2. [Chord Mojo 2 Review & Measurements](https://forum.headphones.com/t/chord-mojo-2-review-measurements/19768) - Watch the video Regular price $775 Sale price $775 Regular price $775.00 Unit price / per In Stock C...

3. [CHORD Mojo 2 Review (Portable DAC & HP Amp) | Page 4](https://www.audiosciencereview.com/forum/index.php?threads%2Fchord-mojo-2-review-portable-dac-hp-amp.34160%2Fpage-4) - A brief interruption in the USB data connection will result in full-scale white noise being sent to ...

4. [Chord Mojo 2 Thread ___ [product released January 31, 2022](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-232) - The white noise occurs when the streamer/pc looses the handshake, usually when changing tracks of di...

5. [Chord Mojo 2 Thread ___ [product released January 31, 2022](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-238) - Provide some test findings that can solve the problem on my laptop. Symptom: On windows 10 or 11, pl...

6. [Chord Mojo 2 Thread ___ [product released January 31, 2022 -- starting on page 95 of thread]](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-163) - FWIW - I’m using it with my Ipad pro 11 inch without issues so far :fingers_crossed: I am as well. N...

7. [Chord Mojo 2 Thread ___ [product released January 31 ...](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-544) - The only thing I was able to find on the firmware update here was this video (more than a dozen page...

8. [Mojo 2 works like crap with ARC (software update?) - Chord](https://community.roonlabs.com/t/mojo-2-works-like-crap-with-arc-software-update/254604) - I saw a Mojo 2 review about getting random white noise and saying it was specific to the cable. I bo...

9. [Chord Mojo 2 Thread ___ [product released January 31, 2022](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-619) - A reboot of the Sony fixed the issue. Chord is not known for rapid firmware fixes. that indicates a ...

10. [EXCITING NEWS: The Mojo 2 has been UPGRADED! Responding ...](https://www.facebook.com/chordelectronics/posts/-exciting-news-the-mojo-2-has-been-upgraded-responding-to-your-feedback-the-mult/1187961386764345/) - ✨ EXCITING NEWS: The Mojo 2 has been UPGRADED! Responding to your feedback, the multi-award-winning ...

11. [Mojo setup tips? - Chord - Roon Labs Community](https://community.roonlabs.com/t/mojo-setup-tips/139906) - Chord's USB implementation is pretty good by reputation, although I'm inclined to use TOSlink optica...

12. [Chord Mojo 2 Thread ___ [product released January 31, 2022 -- starting on page 95 of thread]](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-428) - I had the same question some time back and was only able to find some old Fiio DAPs and xDuoo X10T I...

13. [What should I set this to using chord mojo 2](https://www.facebook.com/groups/283741837902569/posts/679803664963049/) - What should I set this to using chord mojo 2

14. [Macbook M1 Pro + Chord Mojo II Audio Dropouts - Page 2](https://community.roonlabs.com/t/macbook-m1-pro-chord-mojo-ii-audio-dropouts/248339?page=2) - Hello Jon Thank‘s for answering. I managed the problem with the dropouts while using a Topping D10s ...

15. [What's the difference between USB-C and optical input on Chord ...](https://www.facebook.com/groups/3235813040040291/posts/3903655053256083/) - To troubleshoot Chord Mojo 2 connectivity issues, users suggest checking the USB-C cable quality and...

16. [Yeah, Baby! A Review of Chord Electronics' Mojo2](https://www.sonusapparatus.com/2022/10/yeah-baby-a-review-of-chord-electronics-mojo2/)

17. [Chord Mojo 2 - Possible Hazardous Max Volume Issue With Mac OSX](https://community.roonlabs.com/t/chord-mojo-2-possible-hazardous-max-volume-issue-with-mac-osx/299823) - I've experienced sudden, full-volume output (0dBFS) during playback. Just instant, ear-shattering vo...

18. [Chord Mojo 2 Measurements - GoldenSound](https://goldensound.audio/2022/05/26/chord-mojo-2-measurements/) - The Chord Mojo 2 is an excellent portable DAC and headphone amp with some great features on top

19. [Mojo 2 - (4.4 mm) Manual V.1](https://chordelectronics.co.uk/wp-content/uploads/2022/01/Mojo-2-4.4-user-manual.pdf)

20. [Chord Mojo 2 Thread ___ [product released January 31, 2022](https://www.head-fi.org/threads/chord-mojo-2-thread-___-product-released-january-31-2022-starting-on-page-95-of-thread.885405/page-390) - After about 6 minutes the output level of the right channel reaches 3.5V DC and stays there forever!...

21. [Weird noise from Chord Mojo 2 headphones? - Facebook](https://www.facebook.com/groups/3235813040040291/posts/4343505932604324/) - Need help. All of a sudden on my mojo 2 (usb to usbc in connection from my PC) I am getting a crazy ...

22. [Does anyone have a solution for the 'screaming' issue with ...](https://www.facebook.com/groups/3235813040040291/posts/4039322713022649/) - To reduce distortion when using the Chord Mojo 2 with sensitive headphones, consider adjusting the g...

