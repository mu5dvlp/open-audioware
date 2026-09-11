using System.Collections;
using UnityEngine;
using UnityEngine.UI;

namespace Measurement
{
    /// <summary>
    /// 画面全体を覆う白フラッシュオーバーレイ(docs/measurement-m1.md §2.2 方式A)。
    /// 別端末のスロー動画から、無音のタップ操作の代わりにトリガー時刻を特定するために使う。
    /// <see cref="Flash"/> は呼び出しと同一フレームで即座にアルファを最大へ上げる
    /// (発音要求もこれと同一フレーム内で行うこと。初期構築仕様『§4 機能仕様』の要件)。
    /// </summary>
    [RequireComponent(typeof(Image))]
    public class ScreenFlash : MonoBehaviour
    {
        [SerializeField]
        [Tooltip("フラッシュを表示し続けるフレーム数(1〜2推奨。スロー動画のフレームで判別できれば十分)。")]
        private int flashFrames = 2;

        private Image _image;
        private Coroutine _hideRoutine;

        private void Awake()
        {
            _image = GetComponent<Image>();
            SetAlpha(0f);
        }

        /// <summary>
        /// 画面を即座に白へ(このフレームから可視)。<see cref="flashFrames"/> フレーム後に
        /// 自動的に消灯する。連打時は前回の消灯待ちをキャンセルして最新のタップを優先する。
        /// </summary>
        public void Flash()
        {
            if (_hideRoutine != null)
            {
                StopCoroutine(_hideRoutine);
            }

            SetAlpha(1f);
            _hideRoutine = StartCoroutine(HideAfterFrames(flashFrames));
        }

        private IEnumerator HideAfterFrames(int frames)
        {
            for (int i = 0; i < frames; i++)
            {
                yield return null;
            }

            SetAlpha(0f);
            _hideRoutine = null;
        }

        private void SetAlpha(float alpha)
        {
            Color color = _image.color;
            color.a = alpha;
            _image.color = color;
        }
    }
}
