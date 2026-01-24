import Template from '../../interaction/template'
import Controller from '../../core/controller'
import Scroll from '../../interaction/scroll'
import Api from './api'
import SETTINGS_KEYS from '../../utils/constants'

function Main(){
    let comp
    let scrl = new Scroll({mask:true,over:true,step:200})
    let last

    let menu_visible = {
        'interface': SETTINGS_KEYS.MENU_INTERFACE,
        'content_rows': SETTINGS_KEYS.MENU_CHANNELS,
        'player': SETTINGS_KEYS.MENU_PLAYER,
        'parser': SETTINGS_KEYS.MENU_PARSER,
        'server': SETTINGS_KEYS.MENU_SERVER,
        'tmdb': SETTINGS_KEYS.MENU_TMDB,
        'plugins': SETTINGS_KEYS.MENU_PLUGINS,
        'parental_control': SETTINGS_KEYS.MENU_PARENTAL_CONTROL,
        'data': SETTINGS_KEYS.MENU_DATA,
        'more': SETTINGS_KEYS.MENU_MORE
    }

    function isVisible(name){
        if(menu_visible[name]){
            let value = window.lampa_settings[menu_visible[name]]
            if(value === '0' || value === 0 || value === false || value === 'false') return false
        }
        return true
    }

    /**
     * Создать
     */
    this.create = ()=>{
        comp = Template.get('settings_main')

        if(!window.lampa_settings.account_use) comp.find('[data-component="account"]').remove()
        if(!window.lampa_settings.plugins_use) comp.find('[data-component="plugins"]').remove()
        if(!Lampa.Platform.is('apple_tv')) comp.find('[data-component="appletv"]').remove()

        if(!window.lampa_settings.torrents_use){
            comp.find('[data-component="parser"]').remove()
            comp.find('[data-component="server"]').remove()
        }

        for(let name in menu_visible){
            if(!isVisible(name)){
                comp.find('[data-component="'+name+'"]').remove()
            }
        }

        scrl.render().css('max-height', window.innerWidth <= 480 ? window.innerHeight * 0.6 : 'unset')
        scrl.append(comp)
        scrl.addSwipeDown(this.swipeAction)

        this.update()
    }

    /**
     * Обновить события
     */
    this.update = ()=>{
        let components = Api.allComponents()

        for(let name in components){
            let aded = components[name]

            if(isVisible(name)){
                if(!comp.find('[data-component="'+name+'"]').length){
                    let item = $(`<div class="settings-folder selector" data-component="${name}">
                        <div class="settings-folder__icon">
                            ${aded.icon}
                        </div>
                        <div class="settings-folder__name">${aded.name}</div>
                    </div>`)

                    if(aded.before) comp.find('[data-component="'+aded.before+'"]').before(item)
                    else if(aded.after) comp.find('[data-component="'+aded.after+'"]').after(item)
                    else comp.append(item)
                }
            }
            else{
                comp.find('[data-component="'+name+'"]').remove()
            }
        }

        comp.find('.selector').unbind('hover:focus').on('hover:focus',(event)=>{
            last = event.target

            scrl.update($(event.target),true)
        }).unbind('hover:hover hover:touch').on('hover:hover hover:touch',(event)=>{
            last = event.target

            Navigator.focused(event.target)
        }).not('[data-static]').unbind('hover:enter').on('hover:enter',(event)=>{
            this.render().detach()

            this.onCreate($(event.target).data('component'))
        })
    }

    /**
     * Сделать активным
     */
    this.active = function(){
        Controller.collectionSet(comp)
        Controller.collectionFocus(last,comp)

        scrl.height($('.settings__head'))
    }

    /**
     * Рендер
     * @returns {object}
     */
    this.render = ()=>{
        return scrl.render()
    }
}

export default Main
